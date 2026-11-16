//! Kits + four attach levels (card C27, D49/D50): kit CRUD, attach/detach
//! with add/remove per level, and the effective set of a conversation.
//! Resolution itself is `aionui_db::kit_resolve` (shared with the market gate
//! and the launcher). Design: clauterm-studio/design/skills-mcp/C27-KITS.md.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use aionui_api_types::{
    AttachKitItemRequest, CreateKitRequest, KitAttachmentResponse, KitEffectiveItem, KitEffectiveMcp,
    KitEffectiveQuery, KitEffectiveSetResponse, KitErrorCode, KitItemKind, KitLevel, KitMcpMember, KitMissingTemplate,
    KitOp, KitProvenance, KitResponse, ListKitAttachmentsQuery, UpdateKitRequest,
};
use aionui_db::models::{KitAttachmentRow, KitItemRow, KitRow, McpMarketRow};
use aionui_db::{
    AttachParams, CreateKitParams, DbError, IKitRepository, KIT_NAME_TAKEN_MSG, KIT_STALE_MSG, KitDecision, KitMembers,
    KitScope, UpdateKitParams, kit_resolve,
};
use tracing::{info, warn};

use super::error::{MarketError, kit_error};
use super::kit_seed::DEFAULT_KITS;
use super::rules::GLOBAL_IDS;
use super::service::McpMarketService;
use super::template::load_templates;

const MAX_NAME: usize = 80;
const MAX_DESCRIPTION: usize = 500;
const MAX_MEMBERS: usize = 200;

/// Re-applies a conversation's kit skills to its frozen snapshot and skill
/// view after a conversation-level attach/detach (the conversation service
/// lives in an upper crate, so composition implements this).
#[async_trait::async_trait]
pub trait IKitConversationApplier: Send + Sync {
    async fn reapply_kit_skills(&self, user_id: &str, conversation_id: &str) -> Result<(), String>;
}

/// The repository's conflicts as kit codes (name taken vs stale write).
fn kit_conflict(err: DbError) -> MarketError {
    match err {
        DbError::Conflict(msg) if msg == KIT_STALE_MSG => kit_error(KitErrorCode::Stale, msg),
        DbError::Conflict(msg) if msg == KIT_NAME_TAKEN_MSG => kit_error(KitErrorCode::NameTaken, msg),
        DbError::NotFound(msg) => kit_error(KitErrorCode::NotFound, msg),
        other => other.into(),
    }
}

fn level_from(value: &str) -> KitLevel {
    KitLevel::parse(value).unwrap_or(KitLevel::Conversation)
}

fn kind_from(value: &str) -> KitItemKind {
    KitItemKind::parse(value).unwrap_or(KitItemKind::Skill)
}

fn op_from(value: &str) -> KitOp {
    KitOp::parse(value).unwrap_or(KitOp::Add)
}

fn valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Trim, drop empties, dedupe (order kept).
fn clean_list(values: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    values
        .iter()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty() && seen.insert(v.clone()))
        .collect()
}

/// Whether an MCP instance may be granted at the global level (R2/R3).
fn globally_allowed(row: &McpMarketRow) -> bool {
    GLOBAL_IDS.contains(&row.name.as_str()) && row.market_tier.as_deref() != Some("write")
}

impl McpMarketService {
    fn kits(&self) -> Result<&Arc<dyn IKitRepository>, MarketError> {
        self.kits
            .as_ref()
            .ok_or_else(|| MarketError::Unavailable("kits are not configured".to_owned()))
    }

    async fn instances_by_id(&self, user_id: &str) -> Result<HashMap<String, McpMarketRow>, MarketError> {
        Ok(self
            .repo
            .list_instances(user_id)
            .await?
            .into_iter()
            .map(|r| (r.id.clone(), r))
            .collect())
    }

    // -- kits ---------------------------------------------------------------

    /// Live kits, seeding the KITS.md defaults first (idempotent).
    pub async fn list_kits(&self, user_id: &str) -> Result<Vec<KitResponse>, MarketError> {
        let kits = self.kits()?;
        let seeded = kits.seed_kits(user_id, DEFAULT_KITS).await?;
        if seeded > 0 {
            info!(count = seeded, "kits: default kits seeded");
        }
        let rows = kits.list_kits(user_id).await?;
        let items = kits.list_kit_items(user_id, None).await?;
        let instances = self.instances_by_id(user_id).await?;
        Ok(rows.iter().map(|k| kit_response(k, &items, &instances)).collect())
    }

    pub async fn get_kit(&self, user_id: &str, id: &str) -> Result<KitResponse, MarketError> {
        let kits = self.kits()?;
        let row = kits
            .find_kit(user_id, id)
            .await?
            .ok_or_else(|| kit_error(KitErrorCode::NotFound, format!("kit {id} not found")))?;
        let items = kits.list_kit_items(user_id, Some(&[row.id.clone()])).await?;
        let instances = self.instances_by_id(user_id).await?;
        Ok(kit_response(&row, &items, &instances))
    }

    /// Validate member lists; returns the cleaned lists.
    async fn validate_members(
        &self,
        user_id: &str,
        skills: Option<&[String]>,
        mcps: Option<&[String]>,
        templates: Option<&[String]>,
    ) -> Result<(Option<Vec<String>>, Option<Vec<String>>, Option<Vec<String>>), MarketError> {
        let mut problems = Vec::new();
        let skills = skills.map(clean_list);
        let mcps = mcps.map(clean_list);
        let templates = templates.map(clean_list);
        for list in [&skills, &mcps, &templates].into_iter().flatten() {
            if list.len() > MAX_MEMBERS {
                problems.push(format!("a kit holds at most {MAX_MEMBERS} items per kind"));
            }
        }
        for name in skills.iter().flatten() {
            if !valid_skill_name(name) {
                problems.push(format!("\"{name}\" is not a valid skill name"));
            }
        }
        if let Some(ids) = &mcps
            && !ids.is_empty()
        {
            let instances = self.instances_by_id(user_id).await?;
            for id in ids {
                if !instances.contains_key(id) {
                    problems.push(format!("MCP market instance {id} does not exist"));
                }
            }
        }
        if let Some(ids) = &templates
            && !ids.is_empty()
        {
            match load_templates(self.catalog.as_ref(), user_id).await {
                Ok(known) => {
                    for id in ids {
                        if !known.iter().any(|t| &t.id == id) {
                            problems.push(format!("template {id} is not in the MCP catalogue"));
                        }
                    }
                }
                Err(e) => warn!(error = %e, "kits: catalogue unavailable; template ids not checked"),
            }
        }
        if !problems.is_empty() {
            return Err(kit_error(KitErrorCode::InvalidItem, problems.join("; ")));
        }
        Ok((skills, mcps, templates))
    }

    fn clean_name(name: &str) -> Result<String, MarketError> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > MAX_NAME {
            return Err(kit_error(
                KitErrorCode::InvalidName,
                format!("kit name must be 1-{MAX_NAME} characters"),
            ));
        }
        Ok(name.to_owned())
    }

    fn clean_description(description: Option<&str>) -> Result<Option<String>, MarketError> {
        let description = description.map(str::trim);
        if description.is_some_and(|d| d.chars().count() > MAX_DESCRIPTION) {
            return Err(kit_error(
                KitErrorCode::InvalidName,
                format!("kit description is at most {MAX_DESCRIPTION} characters"),
            ));
        }
        Ok(description.map(str::to_owned))
    }

    pub async fn create_kit(&self, user_id: &str, req: CreateKitRequest) -> Result<KitResponse, MarketError> {
        let kits = self.kits()?;
        let name = Self::clean_name(&req.name)?;
        let description = Self::clean_description(req.description.as_deref())?.unwrap_or_default();
        let (skills, mcps, templates) = self
            .validate_members(
                user_id,
                Some(&req.skills),
                Some(&req.mcp_instance_ids),
                Some(&req.mcp_templates),
            )
            .await?;
        let (skills, mcps, templates) = (
            skills.unwrap_or_default(),
            mcps.unwrap_or_default(),
            templates.unwrap_or_default(),
        );
        let row = kits
            .create_kit(&CreateKitParams {
                user_id,
                name: &name,
                description: &description,
                members: KitMembers {
                    skills: &skills,
                    mcp_instance_ids: &mcps,
                    mcp_templates: &templates,
                },
            })
            .await
            .map_err(kit_conflict)?;
        info!(kit = %row.id, skills = skills.len(), mcps = mcps.len(), "kits: kit created");
        self.get_kit(user_id, &row.id).await
    }

    pub async fn update_kit(&self, user_id: &str, id: &str, req: UpdateKitRequest) -> Result<KitResponse, MarketError> {
        let kits = self.kits()?;
        kits.find_kit(user_id, id)
            .await?
            .ok_or_else(|| kit_error(KitErrorCode::NotFound, format!("kit {id} not found")))?;
        let name = req.name.as_deref().map(Self::clean_name).transpose()?;
        let description = Self::clean_description(req.description.as_deref())?;
        let (skills, mcps, templates) = self
            .validate_members(
                user_id,
                req.skills.as_deref(),
                req.mcp_instance_ids.as_deref(),
                req.mcp_templates.as_deref(),
            )
            .await?;
        // R2/R3: a kit attached globally may only carry global read instances.
        if let Some(ids) = &mcps {
            let global = kits
                .list_attachments(user_id, Some(KitLevel::Global.as_str()), Some(""))
                .await?
                .into_iter()
                .any(|a| a.item_kind == "kit" && a.item_ref == id && a.op == "add");
            if global {
                let instances = self.instances_by_id(user_id).await?;
                let bad: Vec<&str> = ids
                    .iter()
                    .filter_map(|i| instances.get(i))
                    .filter(|r| !globally_allowed(r))
                    .map(|r| r.name.as_str())
                    .collect();
                if !bad.is_empty() {
                    return Err(kit_error(
                        KitErrorCode::GlobalNotAllowed,
                        format!(
                            "this kit is attached globally; {} may not be global (R2/R3)",
                            bad.join(", ")
                        ),
                    ));
                }
            }
        }
        let conversations = self.conversations_with_kit(user_id, id).await?;
        kits.update_kit(
            user_id,
            id,
            &UpdateKitParams {
                name: name.as_deref(),
                description: description.as_deref(),
                skills: skills.as_deref(),
                mcp_instance_ids: mcps.as_deref(),
                mcp_templates: templates.as_deref(),
                expected_updated_at: req.expected_updated_at,
            },
        )
        .await
        .map_err(kit_conflict)?;
        info!(kit = %id, "kits: kit updated");
        if skills.is_some() {
            self.reapply_conversations(user_id, &conversations).await;
        }
        self.get_kit(user_id, id).await
    }

    pub async fn delete_kit(&self, user_id: &str, id: &str) -> Result<(), MarketError> {
        let kits = self.kits()?;
        let conversations = self.conversations_with_kit(user_id, id).await?;
        if !kits.delete_kit(user_id, id).await? {
            return Err(kit_error(KitErrorCode::NotFound, format!("kit {id} not found")));
        }
        info!(kit = %id, "kits: kit deleted with its attachments");
        self.reapply_conversations(user_id, &conversations).await;
        Ok(())
    }

    async fn conversations_with_kit(&self, user_id: &str, kit_id: &str) -> Result<Vec<String>, MarketError> {
        Ok(self
            .kits()?
            .list_attachments(user_id, Some(KitLevel::Conversation.as_str()), None)
            .await?
            .into_iter()
            .filter(|a| a.item_kind == "kit" && a.item_ref == kit_id)
            .map(|a| a.target_id)
            .collect())
    }

    async fn reapply_conversations(&self, user_id: &str, conversations: &[String]) {
        let Some(applier) = &self.kit_applier else {
            return;
        };
        for conversation_id in conversations {
            if let Err(e) = applier.reapply_kit_skills(user_id, conversation_id).await {
                warn!(conversation_id = %conversation_id, error = %e, "kits: skill re-apply failed");
            }
        }
    }

    // -- attachments ----------------------------------------------------------

    pub async fn list_kit_attachments(
        &self,
        user_id: &str,
        query: ListKitAttachmentsQuery,
    ) -> Result<Vec<KitAttachmentResponse>, MarketError> {
        let kits = self.kits()?;
        let rows = kits
            .list_attachments(user_id, query.level.map(KitLevel::as_str), query.target_id.as_deref())
            .await?;
        let kit_names: HashMap<String, String> = kits
            .list_kits(user_id)
            .await?
            .into_iter()
            .map(|k| (k.id, k.name))
            .collect();
        let instances = self.instances_by_id(user_id).await?;
        Ok(rows
            .into_iter()
            .map(|a| attachment_response(a, &kit_names, &instances))
            .collect())
    }

    pub async fn attach_kit_item(
        &self,
        user_id: &str,
        req: AttachKitItemRequest,
    ) -> Result<KitAttachmentResponse, MarketError> {
        let kits = self.kits()?;
        let target = req.target_id.as_deref().map(str::trim).unwrap_or_default().to_owned();
        let item_ref = req.item_ref.trim().to_owned();
        if item_ref.is_empty() {
            return Err(kit_error(KitErrorCode::InvalidItem, "item_ref must not be empty"));
        }
        match req.level {
            KitLevel::Global if !target.is_empty() => {
                return Err(kit_error(
                    KitErrorCode::InvalidTarget,
                    "a global attachment has no target_id",
                ));
            }
            KitLevel::Global => {}
            _ if target.is_empty() => {
                return Err(kit_error(
                    KitErrorCode::InvalidTarget,
                    format!("a {} attachment needs a target_id", req.level.as_str()),
                ));
            }
            KitLevel::Project => {
                if !self.repo.project_exists(&target).await? {
                    return Err(kit_error(
                        KitErrorCode::UnknownTarget,
                        format!("project {target} not found"),
                    ));
                }
            }
            KitLevel::Conversation => {
                if kits.conversation_scope(user_id, &target).await?.is_none() {
                    return Err(kit_error(
                        KitErrorCode::UnknownTarget,
                        format!("conversation {target} not found"),
                    ));
                }
            }
            KitLevel::Assistant => {}
        }
        let instances = self.instances_by_id(user_id).await?;
        let global_add = req.level == KitLevel::Global && req.op == KitOp::Add;
        match req.item_kind {
            KitItemKind::Kit => {
                kits.find_kit(user_id, &item_ref)
                    .await?
                    .ok_or_else(|| kit_error(KitErrorCode::NotFound, format!("kit {item_ref} not found")))?;
                if global_add {
                    let items = kits
                        .list_kit_items(user_id, Some(std::slice::from_ref(&item_ref)))
                        .await?;
                    let bad: Vec<&str> = items
                        .iter()
                        .filter(|i| i.kind == "mcp")
                        .filter_map(|i| instances.get(&i.item_ref))
                        .filter(|r| !globally_allowed(r))
                        .map(|r| r.name.as_str())
                        .collect();
                    if !bad.is_empty() {
                        return Err(kit_error(
                            KitErrorCode::GlobalNotAllowed,
                            format!(
                                "only {} (read) may be global; this kit carries {} (R2/R3)",
                                GLOBAL_IDS.join(", "),
                                bad.join(", ")
                            ),
                        ));
                    }
                }
            }
            KitItemKind::Skill => {
                if !valid_skill_name(&item_ref) {
                    return Err(kit_error(
                        KitErrorCode::InvalidItem,
                        format!("\"{item_ref}\" is not a valid skill name"),
                    ));
                }
            }
            KitItemKind::Mcp => {
                let row = instances.get(&item_ref).ok_or_else(|| {
                    kit_error(
                        KitErrorCode::InvalidItem,
                        format!("MCP market instance {item_ref} does not exist"),
                    )
                })?;
                if global_add && !globally_allowed(row) {
                    return Err(kit_error(
                        KitErrorCode::GlobalNotAllowed,
                        format!(
                            "only {} (read) may be global, not \"{}\" (R2/R3)",
                            GLOBAL_IDS.join(", "),
                            row.name
                        ),
                    ));
                }
            }
        }
        let row = kits
            .upsert_attachment(&AttachParams {
                user_id,
                level: req.level.as_str(),
                target_id: &target,
                item_kind: req.item_kind.as_str(),
                item_ref: &item_ref,
                op: req.op.as_str(),
            })
            .await?;
        info!(level = %row.level, target = %row.target_id, kind = %row.item_kind, op = %row.op, "kits: attached");
        if req.level == KitLevel::Conversation {
            self.reapply_conversations(user_id, std::slice::from_ref(&target)).await;
        }
        let kit_names: HashMap<String, String> = kits
            .list_kits(user_id)
            .await?
            .into_iter()
            .map(|k| (k.id, k.name))
            .collect();
        Ok(attachment_response(row, &kit_names, &instances))
    }

    pub async fn detach_kit_item(&self, user_id: &str, id: &str) -> Result<(), MarketError> {
        let kits = self.kits()?;
        let row = kits.find_attachment(user_id, id).await?.ok_or_else(|| {
            kit_error(
                KitErrorCode::AttachmentNotFound,
                format!("kit attachment {id} not found"),
            )
        })?;
        kits.delete_attachment(user_id, id).await?;
        info!(level = %row.level, target = %row.target_id, kind = %row.item_kind, "kits: detached");
        if row.level == KitLevel::Conversation.as_str() {
            self.reapply_conversations(user_id, std::slice::from_ref(&row.target_id))
                .await;
        }
        Ok(())
    }

    // -- effective set --------------------------------------------------------

    pub async fn kit_effective_set(
        &self,
        user_id: &str,
        query: KitEffectiveQuery,
    ) -> Result<KitEffectiveSetResponse, MarketError> {
        let kits = self.kits()?;
        let non_empty = |v: Option<String>| v.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
        let conversation_id = non_empty(query.conversation_id);
        let (scope, conversation_skills) = match &conversation_id {
            Some(cid) => {
                let scope = kits
                    .conversation_scope(user_id, cid)
                    .await?
                    .ok_or_else(|| kit_error(KitErrorCode::UnknownTarget, format!("conversation {cid} not found")))?;
                let skills = kits.conversation_skills(user_id, cid).await?.map(|s| s.skills);
                (scope, skills)
            }
            None => (
                KitScope {
                    project_id: non_empty(query.project_id),
                    assistant_id: non_empty(query.assistant_id),
                    conversation_id: None,
                },
                None,
            ),
        };
        let layers = kits.load_layers(user_id, &scope).await?;
        let resolution = kit_resolve::resolve(&scope, &layers);
        let kit_name = |id: &Option<String>| id.as_ref().and_then(|i| layers.kits.get(i)).map(|k| k.name.clone());
        let provenance = |d: &KitDecision| KitProvenance {
            level: level_from(d.level),
            op: op_from(d.op),
            kit_id: d.kit_id.clone(),
            kit_name: kit_name(&d.kit_id),
        };
        let instances = self.instances_by_id(user_id).await?;
        let skills = resolution
            .skills
            .iter()
            .map(|(name, d)| KitEffectiveItem {
                kind: KitItemKind::Skill,
                item_ref: name.clone(),
                included: d.included,
                provenance: provenance(d),
                mcp: None,
            })
            .collect();
        let mcps = resolution
            .mcps
            .iter()
            .map(|(id, d)| KitEffectiveItem {
                kind: KitItemKind::Mcp,
                item_ref: id.clone(),
                included: d.included,
                provenance: provenance(d),
                mcp: Some(effective_mcp(instances.get(id), d)),
            })
            .collect();
        // A wanted template is missing when no included instance of that template is in the set.
        let delivered_templates: BTreeSet<&str> = resolution
            .mcps
            .iter()
            .filter(|(_, d)| d.included)
            .filter_map(|(id, _)| instances.get(id))
            .filter_map(|r| r.market_template_id.as_deref())
            .collect();
        let missing_templates = resolution
            .templates
            .iter()
            .filter(|(t, _)| !delivered_templates.contains(t.as_str()))
            .map(|(t, kit)| KitMissingTemplate {
                template_id: t.clone(),
                kit_id: kit.clone(),
                kit_name: layers.kits.get(kit).map(|k| k.name.clone()).unwrap_or_default(),
            })
            .collect();
        Ok(KitEffectiveSetResponse {
            conversation_id,
            project_id: scope.project_id,
            assistant_id: scope.assistant_id,
            skills,
            mcps,
            conversation_skills,
            missing_templates,
        })
    }
}

fn effective_mcp(row: Option<&McpMarketRow>, d: &KitDecision) -> KitEffectiveMcp {
    let Some(row) = row else {
        return KitEffectiveMcp {
            instance_name: String::new(),
            template_id: None,
            status: String::new(),
            environment: None,
            tier: None,
            enforcement: None,
            clis: Vec::new(),
            delivered: false,
            reason: Some("instance no longer exists".to_owned()),
        };
    };
    let status = row.market_status.clone().unwrap_or_default();
    let reason = if !d.included {
        Some(format!("removed at the {} level", d.level))
    } else if status != "active" {
        Some(format!("instance is {status}"))
    } else if d.level == "global" && !globally_allowed(row) {
        Some("not allowed globally (R2/R3)".to_owned())
    } else {
        None
    };
    KitEffectiveMcp {
        instance_name: row.name.clone(),
        template_id: row.market_template_id.clone(),
        status,
        environment: row.market_environment.clone(),
        tier: row.market_tier.clone(),
        enforcement: row.market_enforcement.clone(),
        clis: row
            .market_clis
            .as_deref()
            .and_then(|c| serde_json::from_str(c).ok())
            .unwrap_or_default(),
        delivered: reason.is_none(),
        reason,
    }
}

fn kit_response(row: &KitRow, items: &[KitItemRow], instances: &HashMap<String, McpMarketRow>) -> KitResponse {
    let mine = || items.iter().filter(|i| i.kit_id == row.id);
    let mut skills: Vec<String> = mine()
        .filter(|i| i.kind == "skill")
        .map(|i| i.item_ref.clone())
        .collect();
    skills.sort();
    let mut mcps: Vec<KitMcpMember> = mine()
        .filter(|i| i.kind == "mcp")
        .map(|i| {
            let inst = instances.get(&i.item_ref);
            KitMcpMember {
                instance_id: i.item_ref.clone(),
                instance_name: inst.map(|r| r.name.clone()).unwrap_or_default(),
                missing: inst.is_none(),
            }
        })
        .collect();
    mcps.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
    let mut mcp_templates: Vec<String> = mine()
        .filter(|i| i.kind == "template")
        .map(|i| i.item_ref.clone())
        .collect();
    mcp_templates.sort();
    KitResponse {
        id: row.id.clone(),
        name: row.name.clone(),
        description: row.description.clone(),
        builtin_key: row.builtin_key.clone(),
        skills,
        mcps,
        mcp_templates,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

fn attachment_response(
    a: KitAttachmentRow,
    kit_names: &HashMap<String, String>,
    instances: &HashMap<String, McpMarketRow>,
) -> KitAttachmentResponse {
    let item_name = match a.item_kind.as_str() {
        "kit" => kit_names.get(&a.item_ref).cloned().unwrap_or_default(),
        "mcp" => instances.get(&a.item_ref).map(|r| r.name.clone()).unwrap_or_default(),
        _ => a.item_ref.clone(),
    };
    KitAttachmentResponse {
        level: level_from(&a.level),
        item_kind: kind_from(&a.item_kind),
        op: op_from(&a.op),
        id: a.id,
        target_id: a.target_id,
        item_ref: a.item_ref,
        item_name,
        created_at: a.created_at,
        updated_at: a.updated_at,
    }
}
