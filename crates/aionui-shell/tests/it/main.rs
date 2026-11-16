//! aionui-shell integration tests: one test binary for the crate (TASK-110).
//!
//! Each former `tests/<name>.rs` is the module `<name>` here, so a test is
//! addressed as `<name>::<test>` (e.g. `cargo nextest run -p aionui-shell -E 'test(/^shell_integration::/)'`).
//! A new test file goes in this directory with a `mod` line below.
//! Files that mutate the process environment stay separate binaries in `tests/`.

mod shell_integration;
mod stt_integration;
mod stt_stream_deepgram_integration;
mod stt_stream_openai_integration;
