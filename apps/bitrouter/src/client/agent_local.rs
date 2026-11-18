//! Historical socket name used to verify that the gateway has no local agent listener.

use std::path::{Path, PathBuf};

pub fn socket_path(control_socket: &Path) -> PathBuf {
    control_socket.with_extension("agent.sock")
}
