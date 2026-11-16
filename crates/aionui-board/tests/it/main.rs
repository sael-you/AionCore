//! aionui-board integration tests: one test binary for the crate (TASK-110).
//!
//! Each former `tests/<name>.rs` is the module `<name>` here, so a test is
//! addressed as `<name>::<test>` (e.g. `cargo nextest run -p aionui-board -E 'test(/^logging::/)'`).
//! A new test file goes in this directory with a `mod` line below.
//! Files that mutate the process environment stay separate binaries in `tests/`.

#[path = "../common/mod.rs"]
mod common;

mod logging;
mod service_rules;
mod service_s4;
mod service_summary;
