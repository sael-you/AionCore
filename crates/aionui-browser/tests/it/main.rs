//! aionui-browser integration tests: one test binary for the crate (TASK-110
//! layout). A new test file goes in this directory with a `mod` line below.
//! Files that mutate the process environment stay separate binaries in `tests/`.

mod extension_relay;
mod live_camoufox;
mod live_chrome;
