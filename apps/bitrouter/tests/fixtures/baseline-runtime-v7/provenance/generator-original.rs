//! Capture a journal using the unchanged P0 baseline executable and synthetic input.
//! Chat streaming fixture shape: https://platform.openai.com/docs/api-reference/chat/streaming
use std::error::Error;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, atomic::{AtomicBool, AtomicUsize, Ordering}};
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;
struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        match self.0.try_wait() {
            Ok(Some(_)) => {}
            _ => {
                if let Err(error) = self.0.kill() { eprintln!("owned child cleanup: {error}"); }
                if let Err(error) = self.0.wait() { eprintln!("owned child reap: {error}"); }
            }
        }
    }
}
fn configured(binary: &Path, output: &Path) -> Command {
    let mut command = Command::new(binary);
    command.env_clear().env("PATH", "/usr/bin:/bin").env("BITROUTER_HOME", output)
        .env("TERM", "dumb").env("XDG_DATA_HOME", output.join("data"))
        .env("XDG_CONFIG_HOME", output.join("config")).current_dir(output);
    command
}
fn event(delta: &str, reason: &str) -> String {
    format!("data: {{\"id\":\"baseline-capture\",\"object\":\"chat.completion.chunk\",\"model\":\"test-model\",\"choices\":[{{\"index\":0,\"delta\":{delta},\"finish_reason\":{reason}}}],\"usage\":{{\"prompt_tokens\":11,\"completion_tokens\":7,\"total_tokens\":18}}}}\n\n")
}
fn respond(mut stream: TcpStream, output: &Path, index: usize) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut request = Vec::new();
    let mut byte = [0_u8; 1];
    while !request.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte)?;
        request.push(byte[0]);
        if request.len() > 65536 { return Err("oversized fixture headers".into()); }
    }
    let header = std::str::from_utf8(&request)?;
    if !header.starts_with("POST /chat/completions ") { return Err("unexpected provider route".into()); }
    let mut length = None;
    for line in header.lines() {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") { length = Some(value.trim().parse::<usize>()?); }
        }
    }
    let length = length.ok_or("missing fixture request length")?;
    if length > 4 * 1024 * 1024 { return Err("oversized fixture body".into()); }
    let mut body = vec![0; length];
    stream.read_exact(&mut body)?;
    fs::write(output.join(format!("provider-request-{index}.json")), body)?;
    let response = if index == 0 {
        event(r#"{"role":"assistant","tool_calls":[{"index":0,"id":"read-evidence","type":"function","function":{"name":"read","arguments":"{\"path\":\"evidence.txt\",\"limit\":10}"}}]}"#, "null")
            + &event("{}", "\"tool_calls\"") + "data: [DONE]\n\n"
    } else if index == 1 {
        event(r#"{"role":"assistant","content":"The evidence says baseline persisted payload."}"#, "null")
            + &event("{}", "\"stop\"") + "data: [DONE]\n\n"
    } else { return Err("unexpected additional provider dispatch".into()); };
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response)?;
    stream.flush()?;
    Ok(())
}
fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let binary = PathBuf::from(args.next().ok_or("missing baseline executable")?);
    let output = PathBuf::from(args.next().ok_or("missing output directory")?);
    fs::create_dir(&output)?;
    let workspace = output.join("workspace"); fs::create_dir(&workspace)?;
    fs::write(workspace.join("evidence.txt"), "baseline persisted payload\n")?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?; listener.set_nonblocking(true)?;
    let config = output.join("bitrouter.yaml");
    fs::write(&config, format!("inherit_defaults: false\nserver:\n  listen: '127.0.0.1:0'\n  skip_auth: true\ndatabase:\n  url: 'sqlite://{}?mode=rwc'\nproviders:\n  mock:\n    api_base: 'http://{address}'\n    api_key: fixture\n    api_protocol:\n      - '*': chat_completions\n    models: [{{id: test-model}}]\n", output.join("bitrouter.db").display()))?;
    let stop = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let stop_worker = Arc::clone(&stop); let calls_worker = Arc::clone(&calls); let capture = output.clone();
    let provider = std::thread::spawn(move || -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(90);
        while !stop_worker.load(Ordering::SeqCst) && Instant::now() < deadline {
            match listener.accept() {
                Ok((stream, _)) => respond(stream, &capture, calls_worker.fetch_add(1, Ordering::SeqCst))?,
                Err(error) if error.kind()==std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(10)),
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    });
    let mut server = OwnedChild(configured(&binary, &output).arg("serve").arg("--config").arg(&config)
        .stdout(File::create(output.join("server.stdout"))?).stderr(File::create(output.join("server.stderr"))?).spawn()?);
    // Local client discovery waits for this foreground owner's endpoint.
    std::thread::sleep(Duration::from_secs(1));
    let mut task = OwnedChild(configured(&binary, &output)
        .args(["task", "run", "Read evidence.txt and report its contents.", "--read-only", "--model", "test-model", "--workspace"])
        .arg(&workspace).arg("--config").arg(&config).stdout(File::create(output.join("task.jsonl"))?).stderr(File::create(output.join("task.stderr"))?).spawn()?);
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status)=task.0.try_wait()? { break status; }
        if Instant::now() >= deadline { return Err("baseline task timed out".into()); }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut shutdown=OwnedChild(configured(&binary,&output).arg("stop").arg("--config").arg(&config)
        .stdout(Stdio::null()).stderr(File::create(output.join("stop.stderr"))?).spawn()?);
    let deadline=Instant::now()+Duration::from_secs(15);
    loop {
        if shutdown.0.try_wait()?.is_some() { break; }
        if Instant::now()>=deadline { return Err("baseline stop timed out".into()); }
        std::thread::sleep(Duration::from_millis(20));
    }
    let deadline=Instant::now()+Duration::from_secs(10);
    loop {
        if server.0.try_wait()?.is_some() { break; }
        if Instant::now()>=deadline { return Err("baseline server did not shut down".into()); }
        std::thread::sleep(Duration::from_millis(20));
    }
    stop.store(true,Ordering::SeqCst);
    provider.join().map_err(|_| "provider fixture thread failed")??;
    if !status.success() { return Err(format!("baseline task failed: {status}; inspect private capture logs").into()); }
    if calls.load(Ordering::SeqCst)!=2 { return Err("expected exactly two baseline model calls".into()); }
    println!("captured baseline database at {}; 2 model calls, one read tool, foreground owner stopped",output.display());
    Ok(())
}
