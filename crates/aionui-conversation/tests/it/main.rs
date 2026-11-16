//! aionui-conversation integration tests: one test binary for the crate (TASK-110).
//!
//! Each former `tests/<name>.rs` is the module `<name>` here, so a test is
//! addressed as `<name>::<test>` (e.g. `cargo nextest run -p aionui-conversation -E 'test(/^acp_tool_call_persistence::/)'`).
//! A new test file goes in this directory with a `mod` line below.
//! Files that mutate the process environment stay separate binaries in `tests/`.

mod acp_tool_call_persistence;
mod active_lease;
mod conversation_crud;
mod conversation_extended;
mod force_kill_convergence;
mod kit_skills;
mod response_middleware_integration;
mod runtime_create_routes;
mod skill_load_protocol;
mod stream_relay_tips;
mod stream_relay_tool_call;
mod thinking_persistence;
