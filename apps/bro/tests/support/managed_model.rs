//! A Chat Completions peer for executable tests. Responses depend on the actual
//! incoming assistant/tool history. Each accepted HTTP request has its own receipt.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

pub struct Model {
    pub endpoint: String,
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<Result<()>>>,
    pub requests: Arc<Mutex<Vec<Value>>>,
    pub peak: Arc<AtomicUsize>,
}

impl Model {
    pub fn start(
        reply: impl Fn(&[Value]) -> Result<Value> + Sync + Send + 'static,
    ) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(vec![]));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let reply = Arc::new(reply);
        let join = {
            let (stop, requests, peak) = (stop.clone(), requests.clone(), peak.clone());
            thread::spawn(move || {
                let mut handles = vec![];
                while !stop.load(Ordering::SeqCst) {
                    let mut conn = match listener.accept() {
                        Ok((conn, _)) => conn,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                            continue;
                        }
                        Err(e) => return Err(e.into()),
                    };
                    let (requests, active, peak, reply) = (
                        requests.clone(),
                        active.clone(),
                        peak.clone(),
                        reply.clone(),
                    );
                    handles.push(thread::spawn(move || -> Result<()> {
                        let (request, correlation) = read_request(&conn)?;
                        let messages = request["messages"].as_array().context("messages")?;
                        let number = {
                            let mut seen = requests.lock().map_err(|_| anyhow::anyhow!("request log"))?;
                            seen.push(request.clone());
                            seen.len()
                        };
                        peak.fetch_max(active.fetch_add(1,Ordering::SeqCst)+1,Ordering::SeqCst);
                        write!(conn,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nX-BitRouter-Execution-Id: exec-{number}\r\nX-BitRouter-Request-Id: {correlation}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n")?;
                        conn.flush()?;
                        let delta = reply(messages).inspect_err(|error| eprintln!("model request {number}: {error:#}"))?;
                        active.fetch_sub(1,Ordering::SeqCst);
                        let finish = if delta.get("tool_calls").is_some() {"tool_calls"} else {"stop"};
                        let body = format!("data: {}\n\ndata: [DONE]\n\n", json!({"id":format!("completion-{number}"),"model":"fixture","object":"chat.completion.chunk","created":1,
                            "choices":[{"index":0,"delta":delta,"finish_reason":finish}],"usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15}}));
                        // The active cancellation case intentionally closes its socket before the reply.
                        let _ = write!(conn,"{:x}\r\n{body}\r\n0\r\n\r\n",body.len());
                        Ok(())
                    }));
                }
                for handle in handles {
                    handle
                        .join()
                        .map_err(|_| anyhow::anyhow!("model handler failed"))??;
                }
                Ok(())
            })
        };
        Ok(Self {
            endpoint,
            requests,
            stop,
            join: Some(join),
            peak,
        })
    }
    pub fn finish(mut self) -> Result<Vec<Value>> {
        self.stop.store(true, Ordering::SeqCst);
        self.join
            .take()
            .context("model listener")?
            .join()
            .map_err(|_| anyhow::anyhow!("model listener failed"))??;
        Ok(std::mem::take(
            &mut *self
                .requests
                .lock()
                .map_err(|_| anyhow::anyhow!("request log"))?,
        ))
    }
}

impl Drop for Model {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn read_request(conn: &TcpStream) -> Result<(Value, String)> {
    conn.set_nonblocking(false)?;
    conn.set_read_timeout(Some(Duration::from_secs(15)))?;
    let mut input = BufReader::new(conn);
    let mut headers = String::new();
    loop {
        ensure!(input.read_line(&mut headers)? > 0, "incomplete request");
        if headers.ends_with("\r\n\r\n") {
            break;
        }
    }
    let headers = headers.to_ascii_lowercase();
    ensure!(
        headers.starts_with("post /v1/chat/completions ")
            && headers.contains("authorization: bearer scoped-test-key\r\n"),
        "unexpected SDK request"
    );
    let size: usize = headers
        .lines()
        .find_map(|l| l.strip_prefix("content-length: "))
        .context("length")?
        .parse()?;
    ensure!(size <= 512 * 1024, "live context over budget");
    let mut body = vec![0; size];
    input.read_exact(&mut body)?;
    let id = headers
        .lines()
        .find_map(|line| line.strip_prefix("x-bitrouter-request-id: "))
        .context("missing caller correlation")?;
    Ok((serde_json::from_slice(&body)?, id.to_owned()))
}

pub fn calls(items: Vec<(String, &str, Value)>) -> Value {
    let calls:Vec<_>=items.into_iter().enumerate().map(|(index,(id,name,args))|
        json!({"index":index,"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}})).collect();
    json!({"role":"assistant","tool_calls":calls})
}

pub fn answer(text: &str) -> Value {
    json!({"role":"assistant","content":text})
}
