//! Terminal input driver for the reusable selection surface.

use std::io::{self, IsTerminal};

use anyhow::{Context, Result};
use bitrouter_tui::select::{Action, Item, Select, View};
use futures::StreamExt;

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
