//! Resolve the executable that accompanies the active test archive.
pub(crate) fn bro() -> std::path::PathBuf {
    std::env::var_os("BITROUTER_TEST_BRO")
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_bro"))
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_bro").into())
        .into()
}
