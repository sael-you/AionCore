//! aionui-session integration tests: one test binary for the crate (TASK-110).
//!
//! Each former `tests/<name>.rs` is the module `<name>` here, so a test is
//! addressed as `<name>::<test>` (e.g. `cargo nextest run -p aionui-session -E 'test(/^live_claude_model::/)'`).
//! A new test file goes in this directory with a `mod` line below.
//! Files that mutate the process environment stay separate binaries in `tests/`.

mod live_claude_model;
mod terminate_delegation;
