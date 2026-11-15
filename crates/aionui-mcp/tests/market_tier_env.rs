//! TASK-72 (train-2 review C2 #6): a tier change drops the env keys only the
//! old tier forced; a declared default applies again.

use std::collections::BTreeMap;
use std::sync::Arc;

use aionui_api_types::{CreateMcpMarketInstanceRequest, UpdateMcpMarketInstanceRequest};
use aionui_db::{
    IMcpMarketRepository, IMcpServerRepository, SqliteMcpMarketRepository, SqliteMcpServerRepository,
    init_database_memory,
};
use aionui_mcp::{McpMarketService, StaticTemplateCatalog};
use serde_json::json;

const USER: &str = "system_default_user";

#[tokio::test]
async fn tier_change_drops_the_old_tiers_forced_env() {
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IMcpMarketRepository> = Arc::new(SqliteMcpMarketRepository::new(db.pool().clone()));
    let servers: Arc<dyn IMcpServerRepository> = Arc::new(SqliteMcpServerRepository::new(db.pool().clone()));
    let catalog = StaticTemplateCatalog::new(vec![json!({"id": "flags", "name": "Flags", "market": {
        "transport": "stdio",
        "command": {"command": "npx", "args": ["-y", "flags-mcp@1.0.0"]},
        "env": {
            "ALLOW_WRITES": {"kind": "plain"},
            "MODE": {"kind": "plain", "default": "safe"}
        },
        "tiers": ["read", "write"],
        "tier_env": {"write": {"ALLOW_WRITES": "true", "MODE": "fast"}},
        "enforcement": "tools-only",
        "clis": ["claude"]
    }})]);
    let market = McpMarketService::new(repo, servers, Arc::new(catalog), [5u8; 32]);
    let created = market
        .create_instance(
            USER,
            CreateMcpMarketInstanceRequest {
                name: "flags-write".into(),
                template_id: "flags".into(),
                description: None,
                environment: "sandbox".into(),
                tier: "write".into(),
                status: None,
                clis: vec!["claude".into()],
                plain_env: BTreeMap::new(),
                tools_allow: None,
                tools_block: vec!["drop_everything".into()],
            },
        )
        .await
        .unwrap();
    assert_eq!(created.plain_env.get("ALLOW_WRITES").map(String::as_str), Some("true"));
    assert_eq!(created.plain_env.get("MODE").map(String::as_str), Some("fast"));

    let read = market
        .update_instance(
            USER,
            &created.id,
            UpdateMcpMarketInstanceRequest {
                tier: Some("read".into()),
                tools_allow: Some(vec!["list".into()]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(read.tier, "read");
    assert!(
        !read.plain_env.contains_key("ALLOW_WRITES"),
        "the write tier's forced flag stayed on a read instance: {:?}",
        read.plain_env
    );
    assert_eq!(
        read.plain_env.get("MODE").map(String::as_str),
        Some("safe"),
        "default applies again"
    );
}
