//! aionui-mcp integration tests: one test binary for the crate (TASK-110).
//!
//! Each former `tests/<name>.rs` is the module `<name>` here, so a test is
//! addressed as `<name>::<test>` (e.g. `cargo nextest run -p aionui-mcp -E 'test(/^adapter_integration::/)'`).
//! A new test file goes in this directory with a `mod` line below.
//! Files that mutate the process environment stay separate binaries in `tests/`.

mod adapter_integration;
mod connection_test_integration;
mod file_adapter_integration;
mod kits;
mod market_assignments_atomic;
mod market_catalogue;
mod market_integration;
mod market_lost_update;
mod market_routes_auth;
mod market_tier_env;
mod oauth_integration;
mod service_integration;
mod session_injection_integration;
mod sync_integration;
mod types_integration;
