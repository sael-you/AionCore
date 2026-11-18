//! TASK-177: a paired companion app hears about a newer published build of
//! itself (`update_available`), with a FAKE companion app on the relay.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aionui_api_types::devices::DevicePairingKind;
use aionui_common::now_ms;
use aionui_devices::DevicesService;
use aionui_devices::companion::{CompanionDownloads, DOWNLOAD_TTL};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::relay::{Ws, harness, next, open, send};

const RELEASE_5: &[u8] = b"PK\x03\x04 companion release five";
const RELEASE_6: &[u8] = b"PK\x03\x04 companion release six, a little longer";

fn sha_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// What the Android build script leaves in `<data dir>/devices/companion`.
fn publish(dir: &Path, apk: &[u8], version: &str, code: u64) {
    std::fs::write(dir.join("aionui-companion.apk"), apk).unwrap();
    std::fs::write(
        dir.join("aionui-companion.apk.sha256"),
        format!("{}  aionui-companion.apk\n", sha_hex(apk)),
    )
    .unwrap();
    std::fs::write(dir.join("version.txt"), format!("{version}\n{code}\n")).unwrap();
}

struct Folder(PathBuf);
impl Drop for Folder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn harness_with_folder() -> (Arc<DevicesService>, u16, Folder) {
    let (svc, port) = harness().await;
    let dir = std::env::temp_dir().join(format!("aionui-companion-update-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    svc.set_companion_downloads(Arc::new(CompanionDownloads::new(&dir)));
    (svc, port, Folder(dir))
}

/// Pair a fake companion app; returns its socket (after `welcome`), token and client id.
async fn pair_app(svc: &Arc<DevicesService>, port: u16, version: &str, code: Option<u64>) -> (Ws, String, String) {
    let pairing = svc.pairing_code("u1", DevicePairingKind::Companion, None).unwrap().code;
    let mut frame = json!({"type": "pair", "code": pairing, "kind": "companion", "name": "Pixel",
                           "version": version, "capabilities": ["tap", "foreground"]});
    if let Some(code) = code {
        frame["version_code"] = json!(code);
    }
    let mut ws = open(port, None).await.unwrap();
    send(&mut ws, frame).await;
    let welcome = next(&mut ws).await.unwrap();
    assert_eq!(welcome["type"], "welcome");
    (
        ws,
        welcome["token"].as_str().unwrap().to_owned(),
        welcome["client_id"].as_str().unwrap().to_owned(),
    )
}

/// Reconnect a paired app; returns its socket after `welcome`.
async fn hello_app(port: u16, token: &str, version: &str, code: Option<Value>) -> Ws {
    let mut frame = json!({"type": "hello", "token": token, "kind": "companion", "version": version,
                           "capabilities": ["tap", "foreground"]});
    if let Some(code) = code {
        frame["version_code"] = code;
    }
    let mut ws = open(port, None).await.unwrap();
    send(&mut ws, frame).await;
    assert_eq!(next(&mut ws).await.unwrap()["type"], "welcome");
    ws
}

/// The next frame within `within`, or `None` when the socket stays quiet.
async fn quiet_or_next(ws: &mut Ws, within: Duration) -> Option<Value> {
    tokio::time::timeout(within, next(ws)).await.ok().flatten()
}

/// Ping and read up to `pong`: every frame core sent before it.
async fn frames_until_pong(ws: &mut Ws) -> Vec<Value> {
    send(ws, json!({"type": "ping"})).await;
    let mut seen = Vec::new();
    loop {
        let frame = next(ws).await.expect("pong");
        if frame["type"] == "pong" {
            return seen;
        }
        seen.push(frame);
    }
}

fn token_of(frame: &Value) -> String {
    let url = frame["url"].as_str().unwrap();
    url.strip_prefix("/api/devices/companion.apk?t=")
        .expect("path-form url on the existing route")
        .to_owned()
}

/// `GET <path>` on the relay router; returns (status line, body).
async fn http_get(port: u16, path: &str) -> (String, Vec<u8>) {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    (
        head.lines().next().unwrap_or_default().to_owned(),
        raw[split + 4..].to_vec(),
    )
}

#[tokio::test]
async fn an_older_app_is_offered_the_published_build_after_its_first_frame() {
    let (svc, port, folder) = harness_with_folder().await;
    publish(&folder.0, RELEASE_5, "0.2.2", 5);

    // Pairing: the offer follows `welcome`.
    let before = now_ms();
    let (mut ws, token, _) = pair_app(&svc, port, "0.2.1", Some(4)).await;
    let offer = next(&mut ws).await.expect("update_available after pair");
    assert_eq!(offer["type"], "update_available");
    assert_eq!(offer["version"], "0.2.2");
    assert_eq!(offer["version_code"], 5);
    assert_eq!(offer["sha256"], sha_hex(RELEASE_5));
    assert_eq!(offer["size"], RELEASE_5.len());
    let ttl = i64::try_from(DOWNLOAD_TTL.as_millis()).unwrap();
    let expires_at = offer["expires_at"].as_i64().unwrap();
    assert!(
        (before + ttl..=now_ms() + ttl).contains(&expires_at),
        "short-lived link"
    );
    // Exactly the agreed fields (the app's fixture), nothing else.
    let mut keys: Vec<&str> = offer.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["expires_at", "sha256", "size", "type", "url", "version", "version_code"]
    );

    // The link serves the very file the hash names; a wrong token does not.
    let (status, body) = http_get(port, offer["url"].as_str().unwrap()).await;
    assert!(status.contains("200"), "{status}");
    assert_eq!(sha_hex(&body), sha_hex(RELEASE_5));
    let (status, _) = http_get(port, "/api/devices/companion.apk?t=nope").await;
    assert!(!status.contains("200"), "{status}");
    drop(ws);

    // Reconnect (`hello`): offered again, with a fresh link.
    let mut ws = hello_app(port, &token, "0.2.1", Some(json!(4))).await;
    let again = next(&mut ws).await.expect("update_available after hello");
    assert_eq!(again["type"], "update_available");
    assert_eq!(again["version_code"], 5);
    assert_ne!(token_of(&again), token_of(&offer));
    let downloads = svc.companion_downloads().unwrap();
    assert!(downloads.check(&token_of(&again)));
    assert!(!downloads.check(&token_of(&offer)), "one live link per pairing");
}

#[tokio::test]
async fn nothing_is_offered_when_the_app_is_current_unknown_or_not_the_companion() {
    let (svc, port, folder) = harness_with_folder().await;
    publish(&folder.0, RELEASE_5, "0.2.2", 5);
    let quiet = |frames: Vec<Value>, who: &str| {
        assert!(
            frames.iter().all(|f| f["type"] != "update_available"),
            "{who}: {frames:?}"
        );
    };

    // Same build.
    let (mut ws, token, _) = pair_app(&svc, port, "0.2.2", Some(5)).await;
    quiet(frames_until_pong(&mut ws).await, "same version code");
    drop(ws);
    // A newer build than the published one (a sideloaded test build).
    let mut ws = hello_app(port, &token, "0.3.0", Some(json!(9))).await;
    quiet(frames_until_pong(&mut ws).await, "newer app");
    drop(ws);
    // 0.2.1 and older report no version code: core cannot compare.
    let mut ws = hello_app(port, &token, "0.2.1", None).await;
    quiet(frames_until_pong(&mut ws).await, "no version code");
    drop(ws);
    // A version code that is not a positive whole number is no version code.
    for bad in [json!("4"), json!(0), json!(-1), json!(4.5), json!(null)] {
        let mut ws = hello_app(port, &token, "0.2.1", Some(bad.clone())).await;
        quiet(frames_until_pong(&mut ws).await, &format!("version_code {bad}"));
    }

    // The Mac helper is never sent the frame, whatever it reports.
    let code = svc.pairing_code("u1", DevicePairingKind::MacHelper, None).unwrap().code;
    let mut mac = open(port, None).await.unwrap();
    send(
        &mut mac,
        json!({"type": "pair", "code": code, "kind": "mac_helper", "name": "MacBook", "version": "0.1.0",
               "version_code": 1, "capabilities": ["tap"]}),
    )
    .await;
    assert_eq!(next(&mut mac).await.unwrap()["type"], "welcome");
    quiet(frames_until_pong(&mut mac).await, "mac helper");
    send(
        &mut mac,
        json!({"type": "event", "device": "self", "name": "update_check", "data": {}}),
    )
    .await;
    quiet(frames_until_pong(&mut mac).await, "mac helper update_check");

    // No release in the folder, or an inconsistent one: nothing to offer.
    std::fs::write(folder.0.join("aionui-companion.apk"), b"PK\x03\x04 half copied").unwrap();
    let (mut ws, _, _) = pair_app(&svc, port, "0.2.1", Some(4)).await;
    quiet(frames_until_pong(&mut ws).await, "hash file does not match the APK");
}

#[tokio::test]
async fn check_now_answers_with_a_fresh_link_or_not_at_all() {
    let (svc, port, folder) = harness_with_folder().await;
    // Nothing published yet: "check now" is answered with silence.
    let (mut ws, _, _) = pair_app(&svc, port, "0.2.1", Some(4)).await;
    let check = json!({"type": "event", "device": "self", "name": "update_check", "data": {}});
    send(&mut ws, check.clone()).await;
    assert!(frames_until_pong(&mut ws).await.is_empty());

    publish(&folder.0, RELEASE_5, "0.2.2", 5);
    send(&mut ws, check.clone()).await;
    let first = next(&mut ws).await.expect("update_available");
    assert_eq!(first["type"], "update_available");
    assert_eq!(first["version_code"], 5);

    // An expired link: the app asks again and gets a new one.
    send(&mut ws, check).await;
    let second = next(&mut ws).await.expect("update_available");
    assert_ne!(token_of(&first), token_of(&second));
    let (status, body) = http_get(port, second["url"].as_str().unwrap()).await;
    assert!(status.contains("200"), "{status}");
    assert_eq!(body, RELEASE_5);
    let (status, _) = http_get(port, first["url"].as_str().unwrap()).await;
    assert!(!status.contains("200"), "the replaced link is dead: {status}");

    // The app's progress report is taken without an answer and without closing.
    send(
        &mut ws,
        json!({"type": "event", "device": "self", "name": "update",
               "data": {"state": "failed", "version_code": 5, "error": "signature_mismatch"}}),
    )
    .await;
    assert!(frames_until_pong(&mut ws).await.is_empty());
}

#[tokio::test]
async fn a_new_release_reaches_the_connected_apps_it_is_newer_for() {
    let (svc, port, folder) = harness_with_folder().await;
    publish(&folder.0, RELEASE_5, "0.2.2", 5);
    svc.spawn_companion_update_watch_every(Duration::from_millis(50));

    let (mut old, _, _) = pair_app(&svc, port, "0.2.1", Some(4)).await;
    assert_eq!(next(&mut old).await.unwrap()["version_code"], 5, "offered at pair");
    let (mut current, _, _) = pair_app(&svc, port, "0.2.2", Some(5)).await;
    let (mut ahead, _, _) = pair_app(&svc, port, "0.3.0", Some(7)).await;
    // The build found at startup is not announced a second time by the watch.
    assert!(quiet_or_next(&mut old, Duration::from_millis(400)).await.is_none());

    publish(&folder.0, RELEASE_6, "0.2.3", 6);
    for (ws, who) in [(&mut old, "0.2.1"), (&mut current, "0.2.2")] {
        let offer = next(ws).await.unwrap_or_else(|| panic!("{who} hears about 0.2.3"));
        assert_eq!(offer["type"], "update_available", "{who}");
        assert_eq!(offer["version"], "0.2.3");
        assert_eq!(offer["version_code"], 6);
        assert_eq!(offer["sha256"], sha_hex(RELEASE_6));
        assert_eq!(offer["size"], RELEASE_6.len());
    }
    assert!(
        frames_until_pong(&mut ahead).await.is_empty(),
        "an app that is ahead hears nothing"
    );
    // One announcement per release, not one per look at the folder.
    assert!(quiet_or_next(&mut old, Duration::from_millis(400)).await.is_none());
}

#[tokio::test]
async fn a_revoked_pairing_loses_its_link() {
    let (svc, port, folder) = harness_with_folder().await;
    publish(&folder.0, RELEASE_5, "0.2.2", 5);
    let (mut ws, _, client_id) = pair_app(&svc, port, "0.2.1", Some(4)).await;
    let offer = next(&mut ws).await.unwrap();
    let (mut other, _, _) = pair_app(&svc, port, "0.2.1", Some(4)).await;
    let other_offer = next(&mut other).await.unwrap();
    let downloads = svc.companion_downloads().unwrap();
    assert!(downloads.check(&token_of(&offer)));

    svc.start_job("u1", &format!("phone:{client_id}:self"), true)
        .await
        .unwrap();
    let (status, _) = http_get(port, offer["url"].as_str().unwrap()).await;
    assert!(!status.contains("200"), "the revoked app's link is dead: {status}");
    assert!(
        downloads.check(&token_of(&other_offer)),
        "another pairing keeps its link"
    );
}
