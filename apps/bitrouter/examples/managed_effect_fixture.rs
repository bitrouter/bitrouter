//! Cargo-built, explicitly gated native-effect process for managed ownership tests.

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args_os()
        .skip(1)
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    let [marker, ready, gate, done] = args.as_slice() else {
        return Err("expected marker, ready, gate and done paths".into());
    };
    let mut effects = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(marker)?;
    writeln!(effects, "started")?;
    effects.sync_all()?;
    std::fs::write(ready, std::process::id().to_string())?;
    let deadline = Instant::now() + Duration::from_secs(45);
    while !gate.exists() {
        if Instant::now() >= deadline {
            return Err("fixture gate expired".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    writeln!(effects, "completed")?;
    effects.sync_all()?;
    std::fs::write(done, b"done")?;
    Ok(())
}
