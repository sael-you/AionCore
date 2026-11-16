//! aionui-session-message integration tests: one test binary for the crate (TASK-110).
//!
//! Each former `tests/<name>.rs` is the module `<name>` here, so a test is
//! addressed as `<name>::<test>` (e.g. `cargo nextest run -p aionui-session-message -E 'test(/^cancel_hook::/)'`).
//! A new test file goes in this directory with a `mod` line below.
//! Files that mutate the process environment stay separate binaries in `tests/`.

#[path = "../common/mod.rs"]
mod common;

mod cancel_hook;
mod child_result_delivery;
mod delivery_semantics;
mod e2e_cross_session;
mod routes_test;
