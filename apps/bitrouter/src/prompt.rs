//! Canonical line input. Prompts use stderr; the OS owns line editing.

use std::io::{self, IsTerminal, Write};

use anyhow::{Context, Result};

pub(crate) struct Item {
    name: String,
    description: String,
}

impl Item {
    pub(crate) fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
        }
    }
}

pub(crate) fn terminal() -> bool {
    io::stdin().is_terminal() && io::stderr().is_terminal()
}

pub(crate) async fn select(
    title: &str,
    help: &str,
    items: Vec<Item>,
    default: usize,
) -> Result<usize> {
    require_terminal()?;
    anyhow::ensure!(
        !items.is_empty() && default < items.len(),
        "no choices available for {title}"
    );
    let mut visible: Vec<_> = (0..items.len()).collect();
    loop {
        eprintln!("\n{title}\n{help}");
        for &index in &visible {
            let item = &items[index];
            eprintln!(
                "  {}. {}{} — {}",
                index + 1,
                item.name,
                if index == default { " [default]" } else { "" },
                item.description
            );
        }
        eprint!(
            "Number/name, or keyword + Enter to filter (Enter = {}: {}, / = all): ",
            default + 1,
            items[default].name
        );
        io::stderr().flush()?;
        let input = read_line().await?;
        let input = input.trim();
        if input.is_empty() {
            return Ok(default);
        }
        if input == "/" {
            visible = (0..items.len()).collect();
            continue;
        }
        if let Ok(number) = input.parse::<usize>() {
            if number > 0 && number <= items.len() {
                return Ok(number - 1);
            }
            eprintln!("Choose a number from 1 to {}.", items.len());
            continue;
        }
        if let Some(index) = items
            .iter()
            .position(|item| item.name.eq_ignore_ascii_case(input))
        {
            return Ok(index);
        }
        let query = input.to_lowercase();
        let matches: Vec<_> = items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                item.name.to_lowercase().contains(&query)
                    || item.description.to_lowercase().contains(&query)
            })
            .map(|(index, _)| index)
            .collect();
        if matches.is_empty() {
            eprintln!("No matches. Try another name or keyword.");
        } else {
            visible = matches;
        }
    }
}

pub(crate) async fn confirm(question: &str, default: bool) -> Result<bool> {
    require_terminal()?;
    loop {
        eprint!("{question} {} ", if default { "[Y/n]" } else { "[y/N]" });
        io::stderr().flush()?;
        match read_line().await?.trim().to_ascii_lowercase().as_str() {
            "" => return Ok(default),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => eprintln!("Enter yes or no."),
        }
    }
}

pub(crate) async fn secret(label: &str) -> Result<String> {
    require_terminal()?;
    let _echo = EchoGuard::hide()?;
    eprint!("{label} (input hidden): ");
    io::stderr().flush()?;
    let result = read_line().await;
    eprintln!();
    let line = result?;
    anyhow::ensure!(
        !line.trim().is_empty(),
        "no API key entered — aborting login"
    );
    Ok(line.trim().to_string())
}

pub(crate) async fn line(label: &str) -> Result<String> {
    require_terminal()?;
    eprint!("{label}: ");
    io::stderr().flush()?;
    let text = read_line().await?;
    anyhow::ensure!(!text.trim().is_empty(), "input is required");
    Ok(text.trim().to_owned())
}

fn require_terminal() -> Result<()> {
    anyhow::ensure!(
        terminal(),
        "interactive input needs stdin and stderr terminals; use explicit flags or init --yes"
    );
    Ok(())
}

pub(crate) fn cancelled(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<io::Error>().is_some_and(|error| {
            matches!(
                error.kind(),
                io::ErrorKind::Interrupted | io::ErrorKind::UnexpectedEof
            )
        }) || matches!(
            cause.downcast_ref::<crate::gateway::providers::auth::login::LoginError>(),
            Some(crate::gateway::providers::auth::login::LoginError::UserIo(
                _
            ))
        )
    })
}

pub(crate) async fn interruptible<T>(work: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::select! {
        biased;
        result = interruption() => {
            result?;
            Err(io::Error::new(io::ErrorKind::Interrupted, "operation cancelled").into())
        }
        result = work => result,
    }
}

async fn read_line() -> Result<String> {
    interruptible(read_canonical_line()).await
}

// Poll canonical input without a blocking Tokio stdin worker. Dropping an OAuth
// paste future must stop reading, and Ctrl-C must let the runtime shut down.
#[cfg(unix)]
async fn read_canonical_line() -> Result<String> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let stdin = io::stdin();
    let mut bytes = Vec::new();
    loop {
        let mut fds = [PollFd::new(&stdin, PollFlags::IN)];
        match poll(
            &mut fds,
            Some(&Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            }),
        ) {
            Ok(0) | Err(rustix::io::Errno::INTR) => {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                continue;
            }
            result => {
                result?;
            }
        }
        let mut byte = [0];
        match rustix::io::read(&stdin, &mut byte) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "input closed; setup cancelled",
                )
                .into());
            }
            Ok(_) if byte[0] == b'\n' => {
                return String::from_utf8(bytes).context("input must be UTF-8");
            }
            Ok(_) => bytes.push(byte[0]),
            Err(rustix::io::Errno::INTR | rustix::io::Errno::AGAIN) => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(windows)]
async fn read_canonical_line() -> Result<String> {
    // One receiver survives cancellation of an OAuth paste future, so its
    // reader cannot steal the next menu's line. This thread never blocks the
    // Tokio runtime's shutdown; terminal mode belongs to the async caller.
    static INPUT: std::sync::OnceLock<
        tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<io::Result<String>>>,
    > = std::sync::OnceLock::new();
    let input = INPUT.get_or_init(|| {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        std::thread::spawn(move || {
            loop {
                let mut line = String::new();
                let result = io::stdin().read_line(&mut line).and_then(|read| {
                    if read == 0 {
                        Err(io::Error::new(io::ErrorKind::UnexpectedEof, "input closed"))
                    } else {
                        Ok(line)
                    }
                });
                let failed = result.is_err();
                if tx.send(result).is_err() || failed {
                    break;
                }
            }
        });
        tokio::sync::Mutex::new(rx)
    });
    Ok(input
        .lock()
        .await
        .recv()
        .await
        .context("console input closed")??)
}

#[cfg(unix)]
struct EchoGuard(rustix::termios::Termios);

#[cfg(unix)]
impl EchoGuard {
    fn hide() -> Result<Self> {
        use rustix::termios::{LocalModes, OptionalActions, tcgetattr, tcsetattr};
        let saved = tcgetattr(io::stdin())?;
        let mut hidden = saved.clone();
        hidden
            .local_modes
            .remove(LocalModes::ECHO | LocalModes::ECHONL);
        tcsetattr(io::stdin(), OptionalActions::Now, &hidden)?;
        Ok(Self(saved))
    }
}

#[cfg(unix)]
impl Drop for EchoGuard {
    fn drop(&mut self) {
        let _ =
            rustix::termios::tcsetattr(io::stdin(), rustix::termios::OptionalActions::Now, &self.0);
    }
}

#[cfg(windows)]
struct EchoGuard(u32);

#[cfg(windows)]
impl EchoGuard {
    fn hide() -> Result<Self> {
        let saved = winapi_util::console::mode(io::stdin())?;
        // ENABLE_ECHO_INPUT, leaving canonical line and processed input enabled.
        winapi_util::console::set_mode(io::stdin(), saved & !0x0004)?;
        Ok(Self(saved))
    }
}

#[cfg(windows)]
impl Drop for EchoGuard {
    fn drop(&mut self) {
        let _ = winapi_util::console::set_mode(io::stdin(), self.0);
    }
}

async fn interruption() -> io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! { result = tokio::signal::ctrl_c() => result, _ = terminate.recv() => Ok(()) }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}
