//! aionui-system integration tests: one test binary for the crate (TASK-110).
//!
//! Each former `tests/<name>.rs` is the module `<name>` here, so a test is
//! addressed as `<name>::<test>` (e.g. `cargo nextest run -p aionui-system -E 'test(/^connection_test_integration::/)'`).
//! A new test file goes in this directory with a `mod` line below.
//! Files that mutate the process environment stay separate binaries in `tests/`.

mod connection_test_integration;
mod current_user_routes;
mod feedback_diagnostics_routes;
mod model_fetch_routes;
mod protocol_detection_routes;
mod provider_routes;
mod settings_routes;
mod system_info_routes;
