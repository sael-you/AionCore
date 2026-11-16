//! aionui-team integration tests: one test binary for the crate (TASK-110).
//!
//! Each former `tests/<name>.rs` is the module `<name>` here, so a test is
//! addressed as `<name>::<test>` (e.g. `cargo nextest run -p aionui-team -E 'test(/^e2e_smoke::/)'`).
//! A new test file goes in this directory with a `mod` line below.
//! Files that mutate the process environment stay separate binaries in `tests/`.

#[path = "../common/mod.rs"]
mod common;

mod e2e_smoke;
mod e2e_team_flow;
mod mailbox_integration;
mod mcp_server_integration;
mod prompts_events_integration;
mod scheduler_integration;
mod session_service_integration;
mod task_board_integration;
