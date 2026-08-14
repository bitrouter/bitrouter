//! Input-path conformance: what the wrapper *sends* to a hosted child.
//!
//! [`super::term`]'s fixture replay covers the output direction — bytes in,
//! grid out. This covers the other one, which had no end-to-end coverage at
//! all: a keypress, a mouse event, or a paste is encoded by `term.rs` and
//! written to the PTY by `pty.rs`, and until now nothing asserted the child
//! received the right bytes. Four of the six fidelity-matrix rows live on this
//! side, and they fail *silently* — the wrapper shipped once with mouse
//! capture and bracketed paste never enabled, and no test could have caught it.
//!
//! ## The trick: `cat -v` is the conformance child
//!
//! Hosted under the wrapper with the line discipline in raw mode, `cat -v`
//! echoes everything it receives using caret notation — an up arrow comes back
//! as the literal text `^[[A`. That text lands in our own grid, so the
//! assertion is a plain string match on what the child *actually got*. No new
//! binary, no new dependency, and it exercises the real `PtyPane` write path
//! rather than a mock.
//!
//! Assertions are on caret-notation text, so a failure reads as
//! `expected "^[[A", grid had "^[OA"` — which names the bug (application
//! cursor mode encoded when it should not have been) rather than dumping hex.

use std::time::{Duration, Instant};

use tokio::sync::mpsc::unbounded_channel;

use super::pty::{HostEvent, PtyLaunch, PtyPane};

/// Raw mode with echo off, so the only thing rendering the input is `cat -v`.
/// Without this the tty's own echo would also print it, and a test could pass
/// on the echo while the child received nothing.
const RAW_CAT: &str = "stty raw -echo; cat -v";

/// Host a shell snippet and return the live pane plus its event channel.
fn host(script: &str) -> (PtyPane, tokio::sync::mpsc::UnboundedReceiver<HostEvent>) {
    let (tx, rx) = unbounded_channel();
    let cwd = std::env::temp_dir();
    let args = ["-c".to_string(), script.to_string()];
    let pane = PtyPane::spawn(
        &PtyLaunch {
            command: "sh",
            args: &args,
            env: &[],
            cwd: &cwd,
        },
        80,
        10,
        tx,
    )
    .expect("spawn conformance child");
    (pane, rx)
}

/// The grid as one string, with trailing padding collapsed.
fn grid(pane: &PtyPane) -> String {
    pane.backend
        .lines(true)
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Pump output until `needle` renders or the deadline passes.
///
/// Polling rather than a fixed sleep: the child's turnaround is microseconds
/// on an idle machine and tens of milliseconds on a loaded one, and a sleep
/// sized for the former is exactly how a suite becomes flaky under CI load.
async fn wait_for(
    pane: &mut PtyPane,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<HostEvent>,
    needle: &str,
) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if grid(pane).contains(needle) {
            return grid(pane);
        }
        match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
            Ok(Some(HostEvent::Output(bytes))) => {
                pane.feed(&bytes);
            }
            Ok(Some(HostEvent::Exited(_))) | Ok(None) => break,
            Err(_) => continue,
        }
    }
    grid(pane)
}

/// Let the child apply a mode it just enabled before we encode against it.
async fn settle(pane: &mut PtyPane, rx: &mut tokio::sync::mpsc::UnboundedReceiver<HostEvent>) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(150), rx.recv()).await {
            Ok(Some(HostEvent::Output(bytes))) => {
                pane.feed(&bytes);
            }
            Ok(Some(HostEvent::Exited(_))) | Ok(None) => return,
            Err(_) => return,
        }
    }
}

fn key(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
    crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
}

#[tokio::test]
async fn arrow_keys_reach_the_child_in_the_mode_it_negotiated() {
    use crossterm::event::KeyCode;

    // Default (normal cursor keys): CSI A.
    let (mut pane, mut rx) = host(RAW_CAT);
    settle(&mut pane, &mut rx).await;
    let bytes = pane.backend.encode_key(&key(KeyCode::Up)).expect("encoded");
    pane.write_input(&bytes);
    let text = wait_for(&mut pane, &mut rx, "^[[A").await;
    assert!(
        text.contains("^[[A"),
        "normal cursor mode must send CSI A; grid was {text:?}"
    );
    pane.kill();

    // After the child enables DECCKM the *same* keypress must encode SS3 O A.
    // This is the property that makes the wrapper transparent: encoding
    // follows what the inner app negotiated, not what we assumed.
    let (mut pane, mut rx) = host("printf '\\033[?1h'; stty raw -echo; cat -v");
    settle(&mut pane, &mut rx).await;
    let bytes = pane.backend.encode_key(&key(KeyCode::Up)).expect("encoded");
    pane.write_input(&bytes);
    let text = wait_for(&mut pane, &mut rx, "^[OA").await;
    assert!(
        text.contains("^[OA"),
        "application cursor mode must send SS3 A; grid was {text:?}"
    );
    pane.kill();
}

#[tokio::test]
async fn a_paste_is_bracketed_only_for_a_child_that_asked() {
    // Unbracketed: a multi-line paste arrives as raw text, which is why a
    // harness that never enabled DEC 2004 sees it as a burst of typing.
    let (mut pane, mut rx) = host(RAW_CAT);
    settle(&mut pane, &mut rx).await;
    let bytes = pane.backend.encode_paste("alpha\nbeta");
    pane.write_input(&bytes);
    let text = wait_for(&mut pane, &mut rx, "beta").await;
    assert!(text.contains("alpha"), "{text:?}");
    assert!(
        !text.contains("^[[200~"),
        "no bracket markers before DEC 2004: {text:?}"
    );
    pane.kill();

    // Bracketed: markers wrap the payload so the child can tell paste from
    // typing and refuse to submit on every embedded newline.
    let (mut pane, mut rx) = host("printf '\\033[?2004h'; stty raw -echo; cat -v");
    settle(&mut pane, &mut rx).await;
    let bytes = pane.backend.encode_paste("alpha\nbeta");
    pane.write_input(&bytes);
    let text = wait_for(&mut pane, &mut rx, "^[[201~").await;
    assert!(
        text.contains("^[[200~") && text.contains("^[[201~"),
        "child must receive both paste markers; grid was {text:?}"
    );
    assert!(text.contains("alpha"), "{text:?}");
    pane.kill();
}

#[tokio::test]
async fn a_mouse_click_reaches_a_child_that_enabled_reporting() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};

    // A child tracking the mouse in SGR (1006) must receive the encoded
    // event. Nothing else in the suite proves the wrapper writes it.
    let (mut pane, mut rx) = host("printf '\\033[?1000h\\033[?1006h'; stty raw -echo; cat -v");
    settle(&mut pane, &mut rx).await;
    assert!(
        pane.backend.mouse_enabled(),
        "the emulator must observe the child's DECSET"
    );
    let bytes = pane
        .backend
        .encode_mouse(
            MouseEventKind::Down(MouseButton::Left),
            3,
            4,
            KeyModifiers::NONE,
        )
        .expect("mouse encodes when the app is tracking");
    pane.write_input(&bytes);
    let text = wait_for(&mut pane, &mut rx, "^[[<0;3;4M").await;
    assert!(
        text.contains("^[[<0;3;4M"),
        "SGR click must reach the child at pane-relative coords; grid was {text:?}"
    );
    pane.kill();
}

#[tokio::test]
async fn a_child_not_tracking_the_mouse_receives_nothing() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};

    // The wheel then belongs to *our* scrollback instead. Sending pointer
    // bytes to an app that never asked would inject junk into its input.
    let (mut pane, mut rx) = host(RAW_CAT);
    settle(&mut pane, &mut rx).await;
    assert!(!pane.backend.mouse_enabled());
    assert!(
        pane.backend
            .encode_mouse(
                MouseEventKind::Down(MouseButton::Left),
                3,
                4,
                KeyModifiers::NONE,
            )
            .is_none(),
        "no encoding without negotiation"
    );
    pane.kill();
}

#[tokio::test]
async fn ctrl_c_reaches_the_child_rather_than_quitting_the_wrapper() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    // The wrapper claims no keybindings: interrupt belongs to the agent.
    let (mut pane, mut rx) = host(RAW_CAT);
    settle(&mut pane, &mut rx).await;
    let bytes = pane
        .backend
        .encode_key(&KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .expect("encoded");
    assert_eq!(bytes, vec![0x03], "legacy interrupt byte");
    pane.write_input(&bytes);
    let text = wait_for(&mut pane, &mut rx, "^C").await;
    assert!(
        text.contains("^C"),
        "child must see the interrupt: {text:?}"
    );
    pane.kill();
}

#[tokio::test]
async fn a_resize_reaches_the_child_as_a_new_window_size() {
    // SIGWINCH propagation, observed from inside the child rather than
    // inferred from our own grid: the child reports the size the kernel gave
    // it, which is the thing a harness re-lays-out against.
    let (mut pane, mut rx) = host("stty raw -echo; dd bs=1 count=1 >/dev/null 2>&1; stty size");
    settle(&mut pane, &mut rx).await;

    pane.resize(100, 24);
    // Any byte releases the child from `dd`, after which it prints its size.
    pane.write_input(b"x");

    let text = wait_for(&mut pane, &mut rx, "24 100").await;
    assert!(
        text.contains("24 100"),
        "child must observe the resized window (rows cols); grid was {text:?}"
    );
    pane.kill();
}
