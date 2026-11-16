//! aionui-api-types integration tests: one test binary for the crate (TASK-110).
//!
//! Each former `tests/<name>.rs` is the module `<name>` here, so a test is
//! addressed as `<name>::<test>` (e.g. `cargo nextest run -p aionui-api-types -E 'test(/^auth_types::/)'`).
//! A new test file goes in this directory with a `mod` line below.
//! Files that mutate the process environment stay separate binaries in `tests/`.

mod auth_types;
mod response_format;
