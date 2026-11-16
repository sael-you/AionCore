//! aionui-ai-agent integration tests: one test binary for the crate (TASK-110).
//!
//! Each former `tests/<name>.rs` is the module `<name>` here, so a test is
//! addressed as `<name>::<test>` (e.g. `cargo nextest run -p aionui-ai-agent -E 'test(/^acp_agent_integration::/)'`).
//! A new test file goes in this directory with a `mod` line below.
//! Files that mutate the process environment stay separate binaries in `tests/`.

mod acp_agent_integration;
mod acp_error_public;
mod acp_module_surface;
mod agent_availability_integration;
#[cfg(feature = "test-support")]
mod agent_types_integration;
mod cc_switch_integration;
mod factory_provider_integration;
mod prompt_pipeline_integration;
