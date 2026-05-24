/// The current Codex CLI version as embedded at compile time.
///
/// Fork release builds can set `CODEX_RELEASE_VERSION` to embed a tag-derived
/// version while keeping source builds on the workspace package version. The
/// injected value should not include a leading `v`; UI surfaces add that prefix
/// where needed.
pub const CODEX_CLI_VERSION: &str = match option_env!("CODEX_RELEASE_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};
