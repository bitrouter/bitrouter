//! Service command name for operator hints. Embedders and unit tests use the
//! canonical package executable. The CLI records its invocation once at startup.

use std::sync::OnceLock;

/// The canonical name the CLI is installed and documented as.
pub const DEFAULT: &str = "bitrouter";

static INVOKED: OnceLock<String> = OnceLock::new();

/// The program name to embed in a "run `… <subcommand>`" hint.
///
/// [`DEFAULT`] until the running binary records something else.
pub fn name() -> &'static str {
    INVOKED.get().map_or(DEFAULT, String::as_str)
}

/// Record the name this process was invoked as, from `argv[0]`'s basename
/// (minus a Windows `.exe` suffix), and return it.
///
/// Falls back to [`DEFAULT`] when `argv[0]` is absent or has no usable
/// basename. Only the first call has an effect; later calls return the name
/// already recorded, so this cannot be used to rewrite hints mid-run.
pub fn record_argv0() -> &'static str {
    if let Some(existing) = INVOKED.get() {
        return existing.as_str();
    }
    let recorded = std::env::args_os()
        .next()
        .as_deref()
        .map(std::path::Path::new)
        .and_then(std::path::Path::file_name)
        .map(|basename| basename.to_string_lossy().into_owned())
        .map(strip_exe_suffix)
        .filter(|basename| !basename.is_empty())
        .unwrap_or_else(|| DEFAULT.to_owned());
    // Losing the race means another thread recorded the same argv[0]; either
    // value is the answer, so take whichever landed.
    let _ = INVOKED.set(recorded);
    name()
}

/// Drop a trailing `.exe` (any casing) so a Windows invocation reports the
/// same name a Unix one does.
fn strip_exe_suffix(basename: String) -> String {
    const EXE: &str = ".exe";
    let Some(cut) = basename.len().checked_sub(EXE.len()) else {
        return basename;
    };
    if basename.is_char_boundary(cut) && basename[cut..].eq_ignore_ascii_case(EXE) {
        let mut trimmed = basename;
        trimmed.truncate(cut);
        return trimmed;
    }
    basename
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_executable_suffix_is_dropped() {
        assert_eq!(strip_exe_suffix("bro.EXE".to_owned()), "bro");
        assert_eq!(strip_exe_suffix("bro".to_owned()), "bro");
        assert_eq!(strip_exe_suffix("exe".to_owned()), "exe");
    }

    #[test]
    fn unrecorded_name_is_the_canonical_spelling() {
        // The CLI binary is the only caller of `record_argv0`, so a unit test
        // binary always sees the default — which is what makes hint assertions
        // in this workspace deterministic.
        assert_eq!(name(), "bitrouter");
    }
}
