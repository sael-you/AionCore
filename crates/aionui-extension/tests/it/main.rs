//! aionui-extension integration tests: one test binary for the crate (TASK-110).
//!
//! Each former `tests/<name>.rs` is the module `<name>` here, so a test is
//! addressed as `<name>::<test>` (e.g. `cargo nextest run -p aionui-extension -E 'test(/^assistant_dispatch_test::/)'`).
//! A new test file goes in this directory with a `mod` line below.
//! Files that mutate the process environment stay separate binaries in `tests/`.

mod assistant_dispatch_test;
mod cron_skill_resolve_test;
mod dependency_management_test;
mod extension_loading_test;
mod hub_test;
mod lifecycle_hooks_test;
mod manifest_validation_test;
mod permission_system_test;
mod registry_test;
mod startup_materialize;
mod state_persistence_test;
