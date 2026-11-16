//! aionui-voice integration tests: one test binary for the crate (TASK-110).
//!
//! Each former `tests/<name>.rs` is the module `<name>` here, so a test is
//! addressed as `<name>::<test>` (e.g. `cargo nextest run -p aionui-voice -E 'test(/^voice_admission_races::/)'`).
//! A new test file goes in this directory with a `mod` line below.
//! Files that mutate the process environment stay separate binaries in `tests/`.

#[cfg(feature = "voice-test")]
#[path = "../common/mod.rs"]
mod common;

#[cfg(feature = "voice-test")]
mod voice_admission_races;
#[cfg(feature = "voice-test")]
mod voice_call_lifecycle;
#[cfg(feature = "voice-test")]
mod voice_conversation_mode;
#[cfg(feature = "voice-test")]
mod voice_drafts;
#[cfg(feature = "voice-test")]
mod voice_http_e2e;
#[cfg(feature = "voice-test")]
mod voice_minimax_cascade;
#[cfg(feature = "voice-test")]
mod voice_speech_adapters;
mod voice_target_evals;
