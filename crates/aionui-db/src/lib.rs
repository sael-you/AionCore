#![warn(clippy::disallowed_types)]

//! SQLite database layer: init, migrations, repository traits, and implementations.
mod agent_binding;
mod database;
mod error;
mod instance_lock;
mod legacy_handoff;
mod migrate_repair;
pub mod models;
mod repository;

pub use agent_binding::{
    AgentBindingResolution, binding_resolution_for_agent, resolve_agent_binding, resolve_agent_binding_for_user,
    resolve_agent_binding_from_rows, runtime_backend_for_agent,
};
pub use database::{
    DATABASE_NEWER_THAN_APP_STAGE, Database, DatabaseInitError, DatabaseInitOptions, init_database,
    init_database_memory, init_database_staged, init_database_staged_with_options, init_database_with_options,
    latest_known_migration_version, maybe_copy_legacy_database, open_database_read_only,
};
pub use error::{
    DbError, SQLITE_BUSY_MESSAGE_MARKERS, SQLITE_UNIQUE_VIOLATION_MARKER, message_indicates_busy,
    message_indicates_unique_violation,
};
pub use instance_lock::{DataDirInstanceGuard, instance_lock_path};
pub use models::{
    AgentMetadataRow, AssistantDefinitionRow, AssistantOverlayRow, AssistantOverrideRow, AssistantPreferenceRow,
    AssistantRow, BoardCardRow, BoardColumnRow, BoardDepRow, BoardDeployJobRow, BoardEventRow, BoardIdempotencyRow,
    BoardLinkRow, BoardLinkViewRow, BoardQuestionRow, BoardReviewRow, BoardSettingsRow, BoardSupervisorCursorRow,
    ConversationArtifactRow, ConversationAssistantSnapshotRow, CreateAssistantParams, ExternalUserProjection,
    FolderRow, IdempotentSendRow, OrderItemType, OrderScene, ProjectExplorerRow, ProjectKind, ProjectRow, Role,
    SkillImportRecordRow, SkillRow, UpdateAgentAvailabilitySnapshotParams, UpdateAgentHandshakeParams,
    UpdateAssistantParams, UpsertAgentMetadataParams, UpsertAssistantDefinitionParams, UpsertAssistantOverlayParams,
    UpsertAssistantPreferenceParams, UpsertConversationAssistantSnapshotParams, UpsertOverrideParams, UserOrderRow,
    UserStatus, UserType, VoiceAuditRow, VoiceDraftRow, VoiceSessionRow, VoiceSettingsRow,
};
pub use repository::channel::UpdatePluginStatusParams;
pub use repository::conversation::{
    ConversationFilters, ConversationRowUpdate, IdempotentInsert, MentionableCandidatesParams, MessagePageCursor,
    MessagePageDirection, MessagePageParams, MessagePageResult, MessageRowUpdate, MessageSearchRow, NewIdempotentSend,
    StaleRuntimeMessageRow, is_valid_extra_key,
};
pub use repository::cron::{
    ClaimCronRunParams, CronRunClaimResult, FinishCronRunParams, RecoverableCronRun, UpdateCronJobParams,
};
pub use repository::mcp_market::{CreateAssignmentParams, MarketInstanceParams, UpdateMarketInstanceParams};
pub use repository::mcp_server::{
    CreateMcpServerParams, GatedMcpRow, MarketDelivery, McpMarketScope, UpdateMcpServerParams,
};
pub use repository::oauth_token::UpsertOAuthTokenParams;
pub use repository::provider::{CreateProviderParams, UpdateProviderParams};
pub use repository::remote_agent::{CreateRemoteAgentParams, UpdateRemoteAgentParams};
pub use repository::skill::{CreateSkillImportRecordParams, UpsertSkillParams};
pub use repository::sqlite_board_recover::{
    BoardRecoverConflict, BoardRecoverCursor, BoardRecoverListedEvent, BoardRecoverRenumber, BoardRecoverReport,
    recover_board,
};
pub use repository::team::{UpdateTaskParams, UpdateTeamParams};
pub use repository::voice::{CreateVoiceSessionParams, NewVoiceAuditEntry, VoiceUsageDelta};
pub use repository::voice_draft::{DraftPatch, DraftTransition, IVoiceDraftRepository, NewVoiceTranscript};
pub use repository::{
    ActivityCursor, ArchiveScope, BOARD_POSITION_GAP, BoardCallerRow, BoardCardCreator, BoardCardCursor,
    BoardCardFilter, BoardConversationInfo, BoardEventQuery, BoardEventScope, BoardEventSeq, BoardPlacement,
    BoardPlacementOutcome, BoardProjectRow, CreateAcpSessionParams, FeedbackDiagnosticsDbContext,
    FeedbackDiagnosticsProfile, FeedbackDiagnosticsProfileResult, FeedbackDiagnosticsRequest,
    FeedbackDiagnosticsResult, IAcpSessionRepository, IAgentMetadataRepository, IAssistantDefinitionRepository,
    IAssistantOverlayRepository, IAssistantOverrideRepository, IAssistantPreferenceRepository, IAssistantRepository,
    IBoardStore, IBoardTx, IChannelRepository, IClientPreferenceRepository, IConversationRepository, ICronRepository,
    IFeedbackDiagnosticsRepository, IMcpMarketRepository, IMcpServerRepository, IOAuthTokenRepository, IProjectStore,
    IProviderRepository, IRemoteAgentRepository, ISettingsRepository, ISidebarStore, ISkillRepository, ITeamRepository,
    IUserOrderStore, IUserRepository, IVoiceAuditRepository, IVoiceSessionRepository, IVoiceSettingsRepository,
    MoveOutcome, NewBoardEvent, OrderItemRef, PageDirection, PersistedSessionState, PinOutcome, PinnedCursor,
    SaveRuntimeStateParams, SidebarConversationThin, SidebarProjectMeta, SidebarTeamThin, SqliteAcpSessionRepository,
    SqliteAgentMetadataRepository, SqliteAssistantDefinitionRepository, SqliteAssistantOverlayRepository,
    SqliteAssistantOverrideRepository, SqliteAssistantPreferenceRepository, SqliteAssistantRepository,
    SqliteBoardStore, SqliteChannelRepository, SqliteClientPreferenceRepository, SqliteConversationRepository,
    SqliteCronRepository, SqliteFeedbackDiagnosticsRepository, SqliteMcpMarketRepository, SqliteMcpServerRepository,
    SqliteOAuthTokenRepository, SqliteProjectStore, SqliteProviderRepository, SqliteRemoteAgentRepository,
    SqliteSettingsRepository, SqliteSidebarStore, SqliteSkillRepository, SqliteTeamRepository, SqliteUserOrderStore,
    SqliteUserRepository, SqliteVoiceRepository, count_live_voice_calls_read_only,
};

// Re-export sqlx pool type for downstream crates
pub use sqlx::SqlitePool;
