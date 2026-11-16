//! aionui-project integration tests: one test binary for the crate (TASK-110).
//!
//! Each former `tests/<name>.rs` is the module `<name>` here, so a test is
//! addressed as `<name>::<test>` (e.g. `cargo nextest run -p aionui-project -E 'test(/^scm_request_path::/)'`).
//! A new test file goes in this directory with a `mod` line below.
//! Files that mutate the process environment stay separate binaries in `tests/`.

mod scm_request_path;
mod service;
