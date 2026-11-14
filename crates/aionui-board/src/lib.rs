#![warn(clippy::disallowed_types)]

//! Built-in project board (Kanban): one board per standard project, shared by
//! every conversation of the project, edited by the owner (user router), the
//! Studio supervisor (`system` actor) and agents (agent router / helper CLI)
//! under one rule set (SPEC v1.0 §2–§7, §12), plus the per-project Board
//! Keeper conversation (D40). Every board uses the `studio` flow (D39).

pub mod actor;
mod convert;
pub mod error;
pub mod events;
pub mod packet;
pub mod ports;
pub mod rate_limit;
pub mod recover;
pub mod routes;
pub mod rules;
pub mod runtime_routes;
mod service;
mod service_agent;
mod service_assign;
mod service_cards;
mod service_gate;
mod service_relations;
mod service_reviews;
pub mod state;
pub mod templates;
pub mod validation;

pub use actor::{AgentCaller, BoardActor, SUPERVISOR_REF};
pub use error::{BoardError, BoardErrorCode, BoardResult};
pub use ports::{AssignmentConversation, BoardHostPort, HostError, HostTeam, KeeperConversation};
pub use recover::{
    RECOVER_MARKER_ENV, RecoverError, RecoverOptions, RecoverOutcome, default_marker_path, recover_database,
};
pub use routes::board_routes;
pub use runtime_routes::board_runtime_routes;
pub use service::BoardService;
pub use service_cards::MoveCard;
pub use state::{BoardRouterState, BoardRuntimeRouterState};
