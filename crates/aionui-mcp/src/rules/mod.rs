//! Rules system (card C37, TASK-131; design
//! clauterm-studio/design/rules/C37-RULES.md).
//!
//! - [`compose`]: the block prepended to a session's preset context.
//! - [`service`]: owner CRUD, caps, the effective view.
//! - [`import`]: one-time import from AGENTS.md / CLAUDE.md / GEMINI.md and
//!   the files AionUi writes afterwards (only after the owner confirms).

pub mod compose;
pub mod import;
pub mod service;

pub use compose::{ComposedRules, compose_rules};
pub use service::{RuleError, RuleService};
