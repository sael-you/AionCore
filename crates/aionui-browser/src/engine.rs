//! The browser engines an instance can run (C35 v1): all of the Chromium
//! family, so the pipe-only CDP channel, the bridge filter, the navigation
//! guard, the egress proxy and the panel are the same for every engine. An
//! engine differs only in where it is downloaded from, how that download is
//! verified, the name of its binary and whether it needs an X display.
//!
//! Download chains, each run by hand on 2026-11-17:
//! - **Chrome** (`dl.google.com/linux/chrome/deb`), **Brave**
//!   (`brave-browser-apt-release.s3.brave.com`) and **Edge**
//!   (`packages.microsoft.com/repos/edge`): the vendor's signed apt
//!   repository — pinned primary key → `gpgv` on `InRelease` → sha256 of
//!   `Packages` → sha256 + size of the `.deb`. Pinned fingerprints: Google's
//!   from google.com/linuxrepositories (C30), Brave's from
//!   brave.com/signing-keys ("Linux Package Repositories - Release Channel"),
//!   Microsoft's from learn.microsoft.com/linux/packages. The Brave and Edge
//!   repositories keep old versions; the highest version is taken.
//! - **Chrome for Testing** and **chrome-headless-shell**: zips from
//!   `storage.googleapis.com/chrome-for-testing-public/`, version and URL
//!   from Google's `last-known-good-versions-with-downloads.json`. Google
//!   publishes no signature and no checksum for them, so the check is HTTPS
//!   to Google's hosts plus the pinned URL shape; the sha256 of the zip is
//!   recorded in the install's verification record.

//! - **Camoufox** (`github.com/daijro/camoufox`, TASK-136 K6): a GitHub
//!   release asset; no signature is published, so its sha256 is pinned in
//!   [`crate::camoufox`] and must also equal the digest GitHub's release API
//!   reports. Driven by Playwright over Juggler, not CDP ([`EngineProtocol`]).

use aionui_api_types::{BrowserEngine, BrowserEngineVerification, BrowserPanelKind};

/// A vendor's signed apt repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AptSource {
    pub repo: &'static str,
    /// The suite under `dists/` (`stable` for the browser vendors).
    pub suite: &'static str,
    pub key_url: &'static str,
    /// The pinned PRIMARY key fingerprint that must sign `InRelease`.
    pub primary_fpr: &'static str,
    pub package: &'static str,
    /// The engine's directory inside the extracted package.
    pub tree: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineSource {
    Apt(AptSource),
    /// A Chrome for Testing download: the JSON key and the zip's top directory.
    ChromeForTesting {
        product: &'static str,
        zip_dir: &'static str,
    },
    /// A pinned GitHub release asset (Camoufox; see [`crate::camoufox`]).
    GithubRelease {
        repo: &'static str,
        tag: &'static str,
        asset: &'static str,
        sha256: &'static str,
    },
}

/// How AionCore and agents drive an engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineProtocol {
    /// Chrome DevTools Protocol over a pipe: CDP bridge, screencast panel.
    Cdp,
    /// Playwright over Juggler through the Camoufox sidecar: Playwright
    /// protocol filter ([`crate::pwfilter`]), VNC panel.
    Juggler,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineSpec {
    pub engine: BrowserEngine,
    /// The executable inside the engine's directory.
    pub binary: &'static str,
    pub source: EngineSource,
    /// Headful engines run on their own Xvfb display.
    pub needs_display: bool,
    pub protocol: EngineProtocol,
}

impl EngineSource {
    /// The only hosts a download of this source may touch, redirects
    /// included (HTTPS only).
    pub fn hosts(&self) -> Vec<&'static str> {
        let host = |url: &'static str| {
            url.strip_prefix("https://")
                .and_then(|rest| rest.split('/').next())
                .unwrap_or_default()
        };
        match self {
            Self::Apt(apt) => {
                let mut hosts = vec![host(apt.repo)];
                if !hosts.contains(&host(apt.key_url)) {
                    hosts.push(host(apt.key_url));
                }
                hosts
            }
            Self::ChromeForTesting { .. } => vec![host(CFT_INDEX_URL), host(CFT_DOWNLOAD_PREFIX)],
            // Camoufox: the release API, the asset (github.com redirects to
            // its release-assets host) and Debian's archive for x11vnc. The
            // sidecar's npm packages come through `npm ci` (lock integrity).
            Self::GithubRelease { .. } => vec![
                "api.github.com",
                "github.com",
                "release-assets.githubusercontent.com",
                "objects.githubusercontent.com",
                "deb.debian.org",
            ],
        }
    }
}

pub const GOOGLE_PRIMARY_FPR: &str = "EB4C1BFD4F042F6DDDCCEC917721F63BD38B4796";
pub const BRAVE_PRIMARY_FPR: &str = "DBF1A116C220B8C7164F98230686B78420038257";
pub const MICROSOFT_PRIMARY_FPR: &str = "BC528686B50D79E339D3721CEB3E94ADBE1229CF";

/// Google's index of Chrome for Testing downloads.
pub const CFT_INDEX_URL: &str =
    "https://googlechromelabs.github.io/chrome-for-testing/last-known-good-versions-with-downloads.json";
/// Every Chrome for Testing download lives under this prefix.
pub const CFT_DOWNLOAD_PREFIX: &str = "https://storage.googleapis.com/chrome-for-testing-public/";

const SPECS: [EngineSpec; 6] = [
    EngineSpec {
        engine: BrowserEngine::Chrome,
        binary: "chrome",
        source: EngineSource::Apt(AptSource {
            repo: "https://dl.google.com/linux/chrome/deb",
            key_url: "https://dl.google.com/linux/linux_signing_key.pub",
            primary_fpr: GOOGLE_PRIMARY_FPR,
            suite: "stable",
            package: "google-chrome-stable",
            tree: "opt/google/chrome",
        }),
        needs_display: true,
        protocol: EngineProtocol::Cdp,
    },
    EngineSpec {
        engine: BrowserEngine::ChromeForTesting,
        binary: "chrome",
        source: EngineSource::ChromeForTesting {
            product: "chrome",
            zip_dir: "chrome-linux64",
        },
        needs_display: true,
        protocol: EngineProtocol::Cdp,
    },
    EngineSpec {
        engine: BrowserEngine::Brave,
        binary: "brave",
        source: EngineSource::Apt(AptSource {
            repo: "https://brave-browser-apt-release.s3.brave.com",
            key_url: "https://brave-browser-apt-release.s3.brave.com/brave-browser-archive-keyring.gpg",
            primary_fpr: BRAVE_PRIMARY_FPR,
            suite: "stable",
            package: "brave-browser",
            tree: "opt/brave.com/brave",
        }),
        needs_display: true,
        protocol: EngineProtocol::Cdp,
    },
    EngineSpec {
        engine: BrowserEngine::Edge,
        binary: "msedge",
        source: EngineSource::Apt(AptSource {
            repo: "https://packages.microsoft.com/repos/edge",
            key_url: "https://packages.microsoft.com/keys/microsoft.asc",
            primary_fpr: MICROSOFT_PRIMARY_FPR,
            suite: "stable",
            package: "microsoft-edge-stable",
            tree: "opt/microsoft/msedge",
        }),
        needs_display: true,
        protocol: EngineProtocol::Cdp,
    },
    EngineSpec {
        engine: BrowserEngine::HeadlessShell,
        binary: "chrome-headless-shell",
        source: EngineSource::ChromeForTesting {
            product: "chrome-headless-shell",
            zip_dir: "chrome-headless-shell-linux64",
        },
        needs_display: false,
        protocol: EngineProtocol::Cdp,
    },
    EngineSpec {
        engine: BrowserEngine::Camoufox,
        binary: "camoufox-bin",
        source: EngineSource::GithubRelease {
            repo: crate::camoufox::REPO,
            tag: crate::camoufox::TAG,
            asset: crate::camoufox::ASSET,
            sha256: crate::camoufox::ASSET_SHA256,
        },
        needs_display: true,
        protocol: EngineProtocol::Juggler,
    },
];

pub fn spec(engine: BrowserEngine) -> &'static EngineSpec {
    SPECS.iter().find(|s| s.engine == engine).unwrap_or(&SPECS[0])
}

impl EngineSpec {
    pub fn verification(&self) -> BrowserEngineVerification {
        match self.source {
            EngineSource::Apt(_) => BrowserEngineVerification::SignedRepository,
            EngineSource::ChromeForTesting { .. } => BrowserEngineVerification::TlsOnly,
            EngineSource::GithubRelease { .. } => BrowserEngineVerification::PinnedSha256,
        }
    }

    pub fn panel(&self) -> BrowserPanelKind {
        match self.protocol {
            EngineProtocol::Cdp => BrowserPanelKind::Screencast,
            EngineProtocol::Juggler => BrowserPanelKind::Vnc,
        }
    }

    /// No window and a headless User-Agent: for agent-only instances.
    pub fn agents_only(&self) -> bool {
        !self.needs_display
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_api_types::BROWSER_ENGINES;

    #[test]
    fn every_engine_has_its_own_spec_over_https() {
        for engine in BROWSER_ENGINES {
            let s = spec(engine);
            assert_eq!(s.engine, engine, "{engine:?}");
            assert_eq!(BrowserEngine::from_id(engine.id()), Some(engine));
            if let EngineSource::Apt(a) = s.source {
                assert!(a.repo.starts_with("https://") && a.key_url.starts_with("https://"));
                assert_eq!(a.primary_fpr.len(), 40);
            }
        }
        assert_eq!(spec(BrowserEngine::Chrome).source.hosts(), ["dl.google.com"]);
        assert_eq!(
            spec(BrowserEngine::Brave).source.hosts(),
            ["brave-browser-apt-release.s3.brave.com"]
        );
        assert_eq!(spec(BrowserEngine::Edge).source.hosts(), ["packages.microsoft.com"]);
        assert_eq!(
            spec(BrowserEngine::HeadlessShell).source.hosts(),
            ["googlechromelabs.github.io", "storage.googleapis.com"]
        );
        assert_eq!(BrowserEngine::from_id("firefox"), None);
        assert!(spec(BrowserEngine::HeadlessShell).agents_only());
        assert!(!spec(BrowserEngine::Brave).agents_only());
        assert_eq!(
            spec(BrowserEngine::ChromeForTesting).verification(),
            BrowserEngineVerification::TlsOnly
        );
        assert_eq!(
            spec(BrowserEngine::Edge).verification(),
            BrowserEngineVerification::SignedRepository
        );
        let camoufox = spec(BrowserEngine::Camoufox);
        assert_eq!(camoufox.protocol, EngineProtocol::Juggler);
        assert_eq!(camoufox.panel(), BrowserPanelKind::Vnc);
        assert_eq!(camoufox.verification(), BrowserEngineVerification::PinnedSha256);
        assert!(!camoufox.agents_only());
        for engine in BROWSER_ENGINES.into_iter().filter(|e| *e != BrowserEngine::Camoufox) {
            assert_eq!(spec(engine).protocol, EngineProtocol::Cdp, "{engine:?}");
        }
    }
}
