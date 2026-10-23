//! Selection entry point with an optional terminal input driver.

#[cfg(feature = "tui")]
use std::io::{self, IsTerminal};

use crate::presentation::selection::Item;
#[cfg(feature = "tui")]
use crate::tui::select::{Action, Select, View};
#[cfg(feature = "tui")]
use anyhow::Context;
use anyhow::Result;
#[cfg(feature = "tui")]
use futures::StreamExt;

#[cfg(feature = "tui")]
pub(crate) async fn select(
    title: &str,
    help: &str,
    items: Vec<Item>,
    default: usize,
) -> Result<usize> {
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        anyhow::bail!("interactive selection needs a terminal; use explicit flags or init --yes");
    }
    if items.is_empty() {
        anyhow::bail!("no choices available for {title}");
    }
    let mut select = Select::new(items, default);
    let mut view = View::open().context("opening selection")?;
    let mut events = crossterm::event::EventStream::new();
    let interrupted = interruption();
    tokio::pin!(interrupted);
    loop {
        view.draw(&mut select, title, help)?;
        let event = tokio::select! {
            event = events.next() => event.context("terminal input closed")??,
            result = &mut interrupted => {
                result?;
                return Err(io::Error::new(io::ErrorKind::Interrupted, "selection cancelled").into());
            }
        };
        match select.handle(event) {
            Action::Selected(index) => return Ok(index),
            Action::Cancelled => {
                return Err(
                    io::Error::new(io::ErrorKind::Interrupted, "selection cancelled").into(),
                );
            }
            Action::Pending => {}
        }
    }
}

#[cfg(not(feature = "tui"))]
pub(crate) async fn select(
    title: &str,
    _help: &str,
    _items: Vec<Item>,
    _default: usize,
) -> Result<usize> {
    anyhow::bail!(
        "interactive selection for {title} requires the tui feature; use explicit flags or init --yes"
    );
}

#[cfg(feature = "tui")]
async fn interruption() -> io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}
