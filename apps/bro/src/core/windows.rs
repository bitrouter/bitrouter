//! Windows directory ownership and npm launcher discovery.

use std::{
    ffi::c_void,
    fs,
    io::{self, Read},
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    ptr,
};

use windows_sys::Win32::{
    Foundation::{ERROR_INSUFFICIENT_BUFFER, HANDLE},
    Security::{
        EqualSid, GetFileSecurityW, GetSecurityDescriptorOwner, GetTokenInformation,
        OWNER_SECURITY_INFORMATION, SECURITY_MAX_SID_SIZE, TOKEN_USER, TokenUser,
    },
};

pub(super) fn owned_by_user(path: &Path) -> io::Result<bool> {
    let path: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut needed = 0;
    // SAFETY: path is NUL-terminated. This call only reads the required length.
    let error = unsafe {
        GetFileSecurityW(
            path.as_ptr(),
            OWNER_SECURITY_INFORMATION,
            ptr::null_mut(),
            0,
            &mut needed,
        );
        io::Error::last_os_error()
    };
    if error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) || needed == 0 {
        return Err(error);
    }
    let mut descriptor = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
    let descriptor_ptr = descriptor.as_mut_ptr().cast::<c_void>();
    let mut owner = ptr::null_mut();
    let mut owner_defaulted = 0;
    // SAFETY: descriptor is aligned, has at least needed bytes, and lives
    // through comparison of the SID returned inside it.
    unsafe {
        if GetFileSecurityW(
            path.as_ptr(),
            OWNER_SECURITY_INFORMATION,
            descriptor_ptr,
            needed,
            &mut needed,
        ) == 0
            || GetSecurityDescriptorOwner(descriptor_ptr, &mut owner, &mut owner_defaulted) == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    if owner.is_null() {
        return Ok(false);
    }

    // GetCurrentProcessToken() yields this read-only pseudo-handle.
    const CURRENT_PROCESS_TOKEN: HANDLE = -4isize as HANDLE;
    const WORDS: usize =
        (size_of::<TOKEN_USER>() + SECURITY_MAX_SID_SIZE as usize).div_ceil(size_of::<usize>());
    let mut token_info = [0usize; WORDS];
    let mut returned = 0;
    // SAFETY: the aligned buffer holds TOKEN_USER and the longest valid SID.
    // On success both SID pointers refer to buffers still owned by this call.
    unsafe {
        if GetTokenInformation(
            CURRENT_PROCESS_TOKEN,
            TokenUser,
            token_info.as_mut_ptr().cast(),
            size_of::<[usize; WORDS]>() as u32,
            &mut returned,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let user = &*token_info.as_ptr().cast::<TOKEN_USER>();
        Ok(EqualSid(owner, user.User.Sid) != 0)
    }
}

pub(super) fn npm_launcher(shim: &Path) -> Option<PathBuf> {
    const MAX_SHIM: u64 = 8 * 1024;
    if shim.file_name()? != "bitrouter.cmd" || !shim.symlink_metadata().ok()?.is_file() {
        return None;
    }
    let mut contents = String::new();
    fs::File::open(shim)
        .ok()?
        .take(MAX_SHIM + 1)
        .read_to_string(&mut contents)
        .ok()?;
    if contents.len() as u64 > MAX_SHIM {
        return None;
    }
    let directory = shim.parent()?;
    let modules =
        if directory.file_name()? == ".bin" && directory.parent()?.file_name()? == "node_modules" {
            if !shim_points_to(&contents, r"..\bitrouter-bro\core\run-bitrouter.js") {
                return None;
            }
            directory.parent()?.to_owned()
        } else {
            if !shim_points_to(
                &contents,
                r"node_modules\bitrouter-bro\core\run-bitrouter.js",
            ) {
                return None;
            }
            directory.join("node_modules")
        };
    let launcher = modules
        .canonicalize()
        .ok()?
        .join("bitrouter-bro/core/run-bitrouter.js");
    if launcher.canonicalize().ok()? != launcher || !launcher.is_file() {
        return None;
    }
    Some(launcher)
}

fn shim_points_to(contents: &str, relative: &str) -> bool {
    contents.lines().any(|line| {
        let Some((command, target)) = line
            .trim()
            .strip_suffix(" %*")
            .and_then(|invocation| invocation.rsplit_once(" \""))
        else {
            return false;
        };
        let target = target
            .strip_prefix("%dp0%\\")
            .or_else(|| target.strip_prefix("%~dp0\\"));
        matches!(
            command.trim_end(),
            "\"%_prog%\""
                | "endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\""
                // cmd-shim 9.0.2 sets PATHEXT after ending the local scope.
                | "endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & set PATHEXT=%PATHEXT:;.JS;=;% & \"%_prog%\""
        ) && target.and_then(|target| target.strip_suffix('"')) == Some(relative)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmd_shim_references_only_the_expected_launcher() {
        let global = r"node_modules\bitrouter-bro\core\run-bitrouter.js";
        let local = r"..\bitrouter-bro\core\run-bitrouter.js";
        let shim = format!(
            "endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\"  \"%dp0%\\{global}\" %*\r\n"
        );
        assert!(shim_points_to(&shim, global));
        assert!(!shim_points_to(&shim, local));
        assert!(!shim_points_to(
            &format!("REM \"%dp0%\\{global}\" %*"),
            global
        ));
    }

    #[test]
    fn cmd_shim_9_0_2_accepts_the_pathext_invocation_with_the_exact_launcher() {
        let global = r"node_modules\bitrouter-bro\core\run-bitrouter.js";
        let shim = concat!(
            "@ECHO off\r\nGOTO start\r\n:find_dp0\r\nSET dp0=%~dp0\r\nEXIT /b\r\n",
            ":start\r\nSETLOCAL\r\nCALL :find_dp0\r\n\r\n",
            "IF EXIST \"%dp0%\\node.exe\" (\r\n  SET \"_prog=%dp0%\\node.exe\"\r\n",
            ") ELSE (\r\n  SET \"_prog=node\"\r\n)\r\n\r\n",
            "endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & ",
            "set PATHEXT=%PATHEXT:;.JS;=;% & \"%_prog%\"  ",
            "\"%dp0%\\node_modules\\bitrouter-bro\\core\\run-bitrouter.js\" %*\r\n",
        );
        assert!(shim_points_to(shim, global));
        assert!(!shim_points_to(
            &shim.replace("run-bitrouter.js", "other.js"),
            global
        ));
        assert!(!shim_points_to(
            &shim.replace("set PATHEXT=%PATHEXT:;.JS;=;%", "arbitrary-command"),
            global
        ));
    }
}
