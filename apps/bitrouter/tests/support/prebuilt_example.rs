//! CI builds process fixtures before starting per-test runtime deadlines.

use std::path::PathBuf;

pub fn from_env(example: &str) -> Result<Option<PathBuf>, String> {
    let Some(directory) =
        std::env::var_os("BITROUTER_TEST_EXAMPLES_DIR").filter(|directory| !directory.is_empty())
    else {
        return Ok(None);
    };
    let path = PathBuf::from(directory).join(format!("{example}{}", std::env::consts::EXE_SUFFIX));
    if !path.is_file() {
        return Err(format!(
            "prebuilt fixture {} is missing; build the selected examples before running tests",
            path.display()
        ));
    }
    path.canonicalize()
        .map(Some)
        .map_err(|error| format!("resolve prebuilt fixture {}: {error}", path.display()))
}
