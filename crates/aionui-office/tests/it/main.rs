//! aionui-office integration tests: one test binary for the crate (TASK-110).
//!
//! Each former `tests/<name>.rs` is the module `<name>` here, so a test is
//! addressed as `<name>::<test>` (e.g. `cargo nextest run -p aionui-office -E 'test(/^conversion_integration::/)'`).
//! A new test file goes in this directory with a `mod` line below.
//! Files that mutate the process environment stay separate binaries in `tests/`.

mod conversion_integration;
mod proxy_integration;
mod watch_manager_integration;
