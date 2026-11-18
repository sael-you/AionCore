//! aionui-db integration tests: one test binary for the crate (TASK-110).
//!
//! Each former `tests/<name>.rs` is the module `<name>` here, so a test is
//! addressed as `<name>::<test>` (e.g. `cargo nextest run -p aionui-db -E 'test(/^acp_registry_contract_migration::/)'`).
//! A new test file goes in this directory with a `mod` line below.
//! Files that mutate the process environment stay separate binaries in `tests/`.

mod acp_registry_contract_migration;
mod adoption_coverage;
mod agent_binding_resolver;
mod agent_skill_delivery_migration;
mod aionrs_fork_capability_migration;
mod antigravity_builtin_agent_migration;
mod assistant_data_unification_schema;
mod assistant_definition_field_removal_migration;
mod board_migration;
mod board_recover;
mod board_repository;
mod channel_repository;
mod client_preference_repository;
mod codex_full_access_mode_migration;
mod conversation_repository;
mod conversation_schema;
mod cron_assistant_first_migration;
mod cron_repository;
mod database_newer_than_app;
mod db_lifecycle;
mod feedback_diagnostics_repository;
mod kit_repository;
mod legacy_handoff;
mod mcp_market_repository;
mod mcp_server_repository;
mod minimax_code_builtin_agent_migration;
mod oauth_token_repository;
mod omp_direct_cli_migration;
mod pi_acp_agent_migration;
mod project_repository;
mod provider_model_settings_migration;
mod provider_repository;
mod registry_binary_agents_migration;
mod registry_npx_agents_migration;
mod remote_agent_repository;
mod server_pool_repository;
mod settings_repository;
mod skill_management_schema;
mod team_capability_criteria_migration;
mod team_repository;
mod usage_repository;
mod user_repository;
mod user_scope_migration;
mod user_scope_pre_migration_repair;
