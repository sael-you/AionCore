//! aionui-app integration tests: one test binary for the crate (TASK-110).
//!
//! Each former `tests/<name>.rs` is the module `<name>` here, so a test is
//! addressed as `<name>::<test>` (e.g. `cargo nextest run -p aionui-app -E 'test(/^acp_config_options_e2e::/)'`).
//! A new test file goes in this directory with a `mod` line below.
//! Files that mutate the process environment stay separate binaries in `tests/`.

#[path = "../common/mod.rs"]
mod common;

mod acp_config_options_e2e;
mod acp_e2e;
mod active_lease_e2e;
mod adoption_assistant_rules_e2e;
mod agent_integration_e2e;
mod agent_provider_health_e2e;
mod aionpro_no_default_user_writes_e2e;
mod assets_e2e;
mod assistants_e2e;
mod auth_e2e;
mod auxiliary_e2e;
mod board_report_skill;
mod board_routes_e2e;
mod browser_extension_route;
mod c11_auth_e2e;
mod capabilities_cli_e2e;
mod channel_e2e;
mod config_cli_e2e;
mod connection_test_e2e;
mod conversation_cli_e2e;
mod conversation_e2e;
mod conversation_runtime_routes_e2e;
mod conversation_skill;
mod cron_e2e;
mod devices_routes_e2e;
mod diagnose_cli_e2e;
mod extension_e2e;
mod file_e2e;
mod health;
mod helper_runtime_auth_e2e;
mod live_ws_http_parity_e2e;
mod local_mode;
mod mcp_crud_e2e;
mod mcp_e2e;
mod mcp_exec_launcher;
mod mcp_market_origin_e2e;
mod memory_routes_e2e;
mod message_e2e;
mod midturn_e2e;
mod office_e2e;
mod open_repro_db;
mod per_user_fs_isolation_e2e;
mod project_page_e2e;
mod remote_agent_e2e;
mod runtime_scope_e2e;
mod runtime_team_tools_e2e;
mod session_revoke_cleanup_e2e;
mod session_skill;
mod shell_e2e;
mod startup_smoke;
mod stt_stream_e2e;
mod system_auth_e2e;
mod system_provider_e2e;
mod system_settings_e2e;
mod system_version_e2e;
mod team_cli_e2e;
mod team_e2e;
mod team_model_e2e;
mod team_phase1_smoke;
mod upgrade_encryption_survives;
mod websocket_e2e;
mod work_dir_e2e;
