//! CLI-owned Windows handle preparation before any subprocess is spawned.
//! https://learn.microsoft.com/windows/win32/api/handleapi/nf-handleapi-sethandleinformation

use std::io;
use std::os::windows::io::AsRawHandle;

use windows_sys::Win32::Foundation::{
    HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
};

/// A redirected child must not retain the launcher's original standard pipes.
/// Command still duplicates the explicitly selected child streams as needed.
pub fn prevent_stdio_inheritance() -> io::Result<()> {
    for handle in [
        io::stdin().as_raw_handle(),
        io::stdout().as_raw_handle(),
        io::stderr().as_raw_handle(),
    ] {
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            continue;
        }
        // SAFETY: these are process-owned standard handles. This metadata-only
        // call changes future inheritance; it neither dereferences nor closes
        // the handles, and the kernel validates them if a stream was replaced.
        if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
