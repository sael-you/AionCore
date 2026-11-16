//! aionui-channel integration tests: one test binary for the crate (TASK-110).
//!
//! Each former `tests/<name>.rs` is the module `<name>` here, so a test is
//! addressed as `<name>::<test>` (e.g. `cargo nextest run -p aionui-channel -E 'test(/^dingtalk_integration::/)'`).
//! A new test file goes in this directory with a `mod` line below.
//! Files that mutate the process environment stay separate binaries in `tests/`.

mod dingtalk_integration;
mod formatter_test;
mod lark_integration;
mod manager_integration;
mod message_service_integration;
mod orchestrator_test;
mod pairing_integration;
mod session_action_integration;
mod stream_relay_test;
mod telegram_integration;
mod weixin_integration;
