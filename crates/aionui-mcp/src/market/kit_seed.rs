//! The default kits K0 (Global, C28) and K1–K12 of `clauterm-studio/design/mcp-catalogue/KITS.md`
//! (v1, 2026-11-15, amended by D50) as data. Skills are library skill names;
//! MCPs are catalogue template ids ("wanted"), never instances: no instance
//! is created by seeding (owner rule), the owner adds instances to a kit once
//! they exist. Seeded per user on first read; a deleted builtin stays deleted.

use aionui_db::SeedKit;

pub const DEFAULT_KITS: &[SeedKit<'static>] = &[
    // C28 (TASK-108): the Global kit, attached at the global level when first
    // seeded. Built-in auto-inject skills (cs-board-report, voice-mode, ...)
    // are delivered anyway and are not listed.
    SeedKit {
        builtin_key: "K0",
        name: "Global",
        description: "Skills and MCPs every conversation gets.",
        skills: &["cs-verify", "cs-delegate"],
        attach_globally: true,
        mcp_templates: &["context7"],
    },
    SeedKit {
        builtin_key: "K1",
        name: "Platform Dev",
        description: "Build AionUi/AionCore (Rust and React/TS) and studio tooling.",
        skills: &[
            "pf-core",
            "pf-ui",
            "cs-build",
            "cs-verify",
            "cs-review-code",
            "cs-delegate",
        ],
        attach_globally: false,
        mcp_templates: &["context7", "github", "deepwiki"],
    },
    SeedKit {
        builtin_key: "K2",
        name: "Ship & Ops",
        description: "Release trains, staging checks, live verification, incident triage.",
        skills: &["pf-youssef-ship", "pf-staging", "cs-verify", "cs-board-butler"],
        attach_globally: false,
        mcp_templates: &["sentry", "cloudflare", "chrome-devtools", "github"],
    },
    SeedKit {
        builtin_key: "K3",
        name: "Web App Backend",
        description: "Schema, migrations, SQL, auth and storage on Supabase.",
        skills: &[],
        attach_globally: false,
        mcp_templates: &["supabase", "sentry"],
    },
    SeedKit {
        builtin_key: "K4",
        name: "Payments",
        description: "Subscriptions, prices, invoices, refunds analysis, MRR.",
        skills: &["pricing-strategy", "churn-prevention", "paywall-upgrade-cro"],
        attach_globally: false,
        mcp_templates: &["paddle", "paddle-sandbox", "stripe"],
    },
    SeedKit {
        builtin_key: "K5",
        name: "Analytics",
        description: "Product and traffic analytics, funnels, experiments readout.",
        skills: &["analytics-tracking", "ab-test-setup"],
        attach_globally: false,
        mcp_templates: &["posthog"],
    },
    SeedKit {
        builtin_key: "K6",
        name: "SEO",
        description: "Technical SEO, indexing, keywords, content gaps, AI-search visibility.",
        skills: &[
            "seo-audit",
            "ai-seo",
            "programmatic-seo",
            "schema-markup",
            "competitor-alternatives",
            "content-strategy",
        ],
        attach_globally: false,
        mcp_templates: &["gsc", "keyword-planner", "chrome-devtools", "firecrawl"],
    },
    SeedKit {
        builtin_key: "K7",
        name: "CRO",
        description: "Landing, signup, onboarding, paywall and form optimization, backed by behaviour data.",
        skills: &[
            "page-cro",
            "signup-flow-cro",
            "onboarding-cro",
            "form-cro",
            "popup-cro",
            "paywall-upgrade-cro",
            "ab-test-setup",
            "marketing-psychology",
        ],
        attach_globally: false,
        mcp_templates: &["posthog", "chrome-devtools"],
    },
    SeedKit {
        builtin_key: "K8",
        name: "Content & Email",
        description: "Copy, sequences, newsletters, lifecycle and transactional email.",
        skills: &[
            "copywriting",
            "copy-editing",
            "email-sequence",
            "cold-email",
            "social-content",
            "content-strategy",
            "voice-extractor",
            "product-marketing-context",
        ],
        attach_globally: false,
        mcp_templates: &["resend", "brevo"],
    },
    SeedKit {
        builtin_key: "K9",
        name: "Ads",
        description: "Google and Meta campaign analysis, creative production, budget recommendations.",
        skills: &["paid-ads", "ad-creative"],
        attach_globally: false,
        mcp_templates: &["google-ads", "meta-ads", "keyword-planner", "ga4", "minimax"],
    },
    SeedKit {
        builtin_key: "K10",
        name: "Research",
        description: "Market, competitor and community research, and web extraction.",
        skills: &[
            "product-marketing-context",
            "competitor-alternatives",
            "launch-strategy",
            "free-tool-strategy",
            "referral-program",
        ],
        attach_globally: false,
        mcp_templates: &["exa", "brave-search", "perplexity", "firecrawl", "reddit", "deepwiki"],
    },
    SeedKit {
        builtin_key: "K11",
        name: "Design",
        description: "Design handoff, UI review, visual assets.",
        skills: &["ad-creative", "social-content"],
        attach_globally: false,
        mcp_templates: &["minimax", "chrome-devtools"],
    },
    SeedKit {
        builtin_key: "K12",
        name: "Docs & Knowledge",
        description: "Bases, sheets, docs and wikis as the source of truth for clients and operations.",
        skills: &["product-marketing-context"],
        attach_globally: false,
        mcp_templates: &["airtable", "google-workspace"],
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thirteen_kits_with_unique_keys_names_and_known_templates() {
        assert_eq!(DEFAULT_KITS.len(), 13);
        // Only K0 is attached globally by the seed (C28).
        let global: Vec<&str> = DEFAULT_KITS
            .iter()
            .filter(|k| k.attach_globally)
            .map(|k| k.builtin_key)
            .collect();
        assert_eq!(global, vec!["K0"]);
        let mut keys: Vec<&str> = DEFAULT_KITS.iter().map(|k| k.builtin_key).collect();
        keys.dedup();
        assert_eq!(keys.len(), 13);
        let mut names: Vec<&str> = DEFAULT_KITS.iter().map(|k| k.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 13);
        // every wanted template exists in the built-in catalogue
        let builtin: Vec<String> = crate::market::builtin_entries()
            .iter()
            .filter_map(|e| e.get("id").and_then(|v| v.as_str()).map(str::to_owned))
            .collect();
        for kit in DEFAULT_KITS {
            for t in kit.mcp_templates {
                assert!(
                    builtin.iter().any(|b| b == t),
                    "{} wants unknown template {t}",
                    kit.builtin_key
                );
            }
        }
    }
}
