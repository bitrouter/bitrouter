//! BitRouter gateway and operator command-line interface.

#[cfg(windows)]
#[path = "main/windows.rs"]
mod windows;

fn main() {
    #[cfg(windows)]
    if let Err(error) = windows::prevent_stdio_inheritance() {
        eprintln!("error: failed to prepare CLI standard handles: {error}");
        std::process::exit(1);
    }
    bitrouter::run_cli();
}
