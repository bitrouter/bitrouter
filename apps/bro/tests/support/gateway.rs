//! Local HTTP fixture for the native worker and CLI boundary tests.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::{Duration, Instant};

pub struct Mock {
    pub endpoint: String,
    requests: thread::JoinHandle<Result<Vec<Value>>>,
}

impl Mock {
    pub fn start(replies: Vec<String>) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        listener.set_nonblocking(true)?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let requests = thread::spawn(move || {
            let mut requests = vec![];
            for reply in replies {
                let deadline = Instant::now() + Duration::from_secs(10);
                let (mut socket, _) = loop {
                    match listener.accept() {
                        Ok(stream) => break stream,
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            thread::sleep(Duration::from_millis(5))
                        }
                        Err(e) => return Err(e.into()),
                    }
                };
                socket.set_nonblocking(false)?;
                socket.set_read_timeout(Some(Duration::from_secs(5)))?;
                let mut reader = BufReader::new(&socket);
                let mut headers = String::new();
                loop {
                    let previous = headers.len();
                    ensure!(reader.read_line(&mut headers)? > 0, "truncated headers");
                    if headers.len() == previous + 2 {
                        break;
                    }
                }
                let headers = headers.to_ascii_lowercase();
                ensure!(
                    headers.starts_with("post /v1/chat/completions ")
                        && headers.contains("authorization: bearer scoped-test-key\r\n"),
                    "wrong SDK endpoint or credentials"
                );
                let length: usize = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .context("missing length")?
                    .parse()?;
                let mut bytes = vec![0; length];
                reader.read_exact(&mut bytes)?;
                requests.push(serde_json::from_slice(&bytes)?);
                write!(socket, "{reply}")?;
            }
            Ok(requests)
        });
        Ok(Self { endpoint, requests })
    }

    pub fn join(self) -> Result<Vec<Value>> {
        self.requests
            .join()
            .map_err(|_| anyhow::anyhow!("mock gateway worker failed"))?
    }
}

pub fn chunk(delta: Value, finish: Option<&str>) -> String {
    format!(
        "data: {}\n\n",
        json!({"id":"completion-1","object":"chat.completion.chunk","model":"fixture",
        "choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
    )
}
pub fn stream(body: String) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}
