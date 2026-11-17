//! Server access pools (card C36, TASK-130; design
//! clauterm-studio/design/servers/C36-SERVER-POOLS.md).
//!
//! - [`service`]: owner CRUD, the write-only private key, the audit list.
//! - [`classifier`]: which commands are crucial.
//! - [`gate`]: the per-call decision of the built-in server MCPs.
//! - [`ssh`]: runs one command over `ssh` with the stored key.

pub mod classifier;
pub mod gate;
pub mod service;
pub mod ssh;

pub use classifier::classify;
pub use gate::{CRUCIAL_SERVER_NAME, ExecChannel, ExecDecision, STANDARD_SERVER_NAME, decide, redact_command};
pub use service::{ServerPoolError, ServerPoolService, granted_servers, server_response};
pub use ssh::{SshOutput, run_ssh};
