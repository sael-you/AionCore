//! C38 / TASK-136 K5: add-ons end to end with a FAKE Mac helper — install and
//! remove from the Devices page, and the gates of a task run (both switches,
//! the owner's approval of the task text, stop on switch-off, audit rows).

use std::sync::Arc;
use std::time::Duration;

use aionui_api_types::devices::{
    DeviceAuditOutcome, DeviceErrorCode, DeviceJobState, DeviceKind, DevicePairingKind, DeviceUpdate,
};
use aionui_devices::{AddonTask, DevicesService};
use serde_json::{Value, json};

use crate::relay::{Ws, harness, next, open, send};

const TASK: &str = "Open the Settings app and report the Android version.";

/// A fake Mac helper that hosts add-ons; `addons` = what its inventory lists.
async fn pair_helper(svc: &Arc<DevicesService>, port: u16, addons: Value) -> (Ws, String) {
    let code = svc.pairing_code("u1", DevicePairingKind::MacHelper, None).unwrap().code;
    let mut ws = open(port, None).await.unwrap();
    send(
        &mut ws,
        json!({"type": "pair", "code": code, "kind": "mac_helper", "name": "MacBook", "version": "0.1.0",
               "capabilities": ["tap", "stop", "addon_install", "addon_remove", "addon_run", "addon_stop"]}),
    )
    .await;
    let welcome = next(&mut ws).await.unwrap();
    let client = welcome["client_id"].as_str().unwrap().to_owned();
    inventory(&mut ws, addons).await;
    (ws, client)
}

async fn inventory(ws: &mut Ws, addons: Value) {
    send(
        ws,
        json!({"type": "inventory",
               "phones": [{"serial": "R5CX", "name": "Galaxy", "transport": "usb", "chrome": true, "enabled": true}],
               "mac": {"name": "MacBook", "permissions_ok": true, "enabled": true},
               "addons": addons}),
    )
    .await;
}

/// The next `request` frame (other frames, e.g. `stop`, are returned in `seen`).
async fn request(ws: &mut Ws, seen: &mut Vec<Value>) -> Value {
    loop {
        let frame = next(ws).await.expect("a frame from core");
        if frame["type"] == "request" {
            return frame;
        }
        seen.push(frame);
    }
}

async fn until<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..500 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("never: {what}");
}

async fn switch(svc: &Arc<DevicesService>, id: &str, on: bool, minutes: Option<u32>) {
    svc.update(
        "u1",
        id,
        &DeviceUpdate {
            enabled: on,
            timeout_minutes: minutes,
        },
    )
    .await
    .unwrap_or_else(|e| panic!("switch {id}: {e}"));
}

fn task(addon: &str, phone: &str) -> AddonTask {
    AddonTask {
        user_id: "u1".into(),
        addon_id: addon.into(),
        phone_id: phone.into(),
        task: TASK.into(),
        max_minutes: Some(5),
        conversation_id: Some("conv-1".into()),
    }
}

async fn audit(svc: &Arc<DevicesService>, addon: &str) -> Vec<(DeviceAuditOutcome, Value)> {
    svc.audit("u1", addon, None, None)
        .await
        .unwrap()
        .rows
        .into_iter()
        .map(|r| {
            assert_eq!(r.verb, "addon_run");
            (
                r.outcome,
                serde_json::from_str(r.detail.as_deref().unwrap_or("null")).unwrap(),
            )
        })
        .collect()
}

#[tokio::test]
async fn an_addon_is_installed_and_removed_through_the_mac_helper() {
    let (svc, port) = harness().await;
    aionui_devices::addons::register(&svc);
    let (mut ws, client) = pair_helper(&svc, port, json!([])).await;
    let installed = format!("addon:{client}:artemis");
    until("the helper's phone is listed", || async {
        svc.list("u1")
            .await
            .unwrap()
            .iter()
            .any(|d| d.kind == DeviceKind::Phone)
    })
    .await;

    // Install from the Library card: one verb on the Mac, the job ends with its answer.
    let job = svc.start_job("u1", "addon:artemis", false).await.unwrap();
    let mut seen = Vec::new();
    let req = request(&mut ws, &mut seen).await;
    assert_eq!(
        (req["verb"].as_str(), req["device"].as_str(), &req["args"]),
        (Some("addon_install"), Some("mac"), &json!({"name": "artemis"}))
    );
    assert_ne!(
        svc.job("u1", &job).unwrap().state,
        DeviceJobState::Done,
        "waits for the Mac"
    );
    send(&mut ws, json!({"type": "response", "id": req["id"], "ok": true, "result": {"name": "artemis", "version": "0123456789ab"}})).await;
    inventory(
        &mut ws,
        json!([{"name": "artemis", "version": "0123456789ab", "disk_bytes": 123456}]),
    )
    .await;
    until("install job done", || async {
        svc.job("u1", &job).unwrap().state == DeviceJobState::Done
    })
    .await;
    until("the installed add-on card", || async {
        svc.get("u1", &installed).await.is_ok()
    })
    .await;
    let card = svc.get("u1", &installed).await.unwrap();
    assert!(card.installed);
    assert_eq!(
        (card.kind, card.version.as_deref(), card.disk_bytes),
        (DeviceKind::Addon, Some("0123456789ab"), Some(123456))
    );
    assert_eq!(card.addon_host.as_deref(), Some(client.as_str()));
    assert!(!card.enabled, "an add-on is off until the owner switches it on");
    assert!(svc.job("u2", &job).is_err(), "another user never sees the job");

    // Remove: the Mac frees the disk first, then the card is gone.
    let job = svc.start_job("u1", &installed, true).await.unwrap();
    let req = request(&mut ws, &mut seen).await;
    assert_eq!(
        (req["verb"].as_str(), &req["args"]),
        (Some("addon_remove"), &json!({"name": "artemis"}))
    );
    assert!(
        svc.get("u1", &installed).await.is_ok(),
        "still there while the Mac works"
    );
    send(
        &mut ws,
        json!({"type": "response", "id": req["id"], "ok": true, "result": {}}),
    )
    .await;
    until("remove job done", || async {
        svc.job("u1", &job).unwrap().state == DeviceJobState::Done
    })
    .await;
    assert!(svc.get("u1", &installed).await.is_err());

    // A failed install is a failed job with the helper's reason.
    let job = svc.start_job("u1", "addon:mobilerun", false).await.unwrap();
    let req = request(&mut ws, &mut seen).await;
    send(&mut ws, json!({"type": "response", "id": req["id"], "ok": false, "error": {"code": "failed", "message": "uv not found"}})).await;
    until("install job failed", || async {
        svc.job("u1", &job).unwrap().state == DeviceJobState::Failed
    })
    .await;
    assert_eq!(svc.job("u1", &job).unwrap().error.as_deref(), Some("uv not found"));
}

#[tokio::test]
async fn without_a_mac_helper_an_addon_cannot_be_installed() {
    let (svc, _port) = harness().await;
    aionui_devices::addons::register(&svc);
    let job = svc.start_job("u1", "addon:artemis", false).await.unwrap();
    until("job failed", || async {
        svc.job("u1", &job).unwrap().state == DeviceJobState::Failed
    })
    .await;
    assert!(svc.job("u1", &job).unwrap().error.unwrap().contains("Mac helper"));
}

#[tokio::test]
async fn a_task_needs_both_switches_and_the_owners_approval_of_its_text() {
    let (svc, port) = harness().await;
    let (mut ws, client) = pair_helper(&svc, port, json!([{"name": "artemis", "version": "1"}])).await;
    let (addon, phone) = (format!("addon:{client}:artemis"), format!("phone:{client}:R5CX"));
    until("cards", || async {
        svc.get("u1", &addon).await.is_ok() && svc.get("u1", &phone).await.is_ok()
    })
    .await;

    // Add-on off (the default): refused before anyone is asked.
    switch(&svc, &phone, true, Some(15)).await;
    let error = svc.addon_run_task(task(&addon, &phone)).await.unwrap_err();
    assert_eq!(error.code(), Some(DeviceErrorCode::DeviceDisabled));
    assert!(svc.approvals("u1").is_empty());
    // Bad input and other users.
    let mut empty = task(&addon, &phone);
    empty.task = "   ".into();
    assert_eq!(
        svc.addon_run_task(empty).await.unwrap_err().code(),
        Some(DeviceErrorCode::DeviceBadMessage)
    );
    let mut other = task(&addon, &phone);
    other.user_id = "u2".into();
    assert_eq!(
        svc.addon_run_task(other).await.unwrap_err().code(),
        Some(DeviceErrorCode::DeviceNotFound)
    );

    switch(&svc, &addon, true, None).await;

    // The owner sees the full task text; a deny never reaches the Mac.
    let svc2 = svc.clone();
    let (a, p) = (addon.clone(), phone.clone());
    let run = tokio::spawn(async move { svc2.addon_run_task(task(&a, &p)).await });
    until("the approval is open", || async { !svc.approvals("u1").is_empty() }).await;
    let approval = svc.approvals("u1").remove(0);
    assert_eq!(approval.task.as_deref(), Some(TASK), "the full task text");
    assert_eq!(
        (approval.verb.as_str(), approval.device_id.as_str()),
        ("addon_run", addon.as_str())
    );
    assert!(svc.approvals("u2").is_empty(), "only the owner is asked");
    svc.decide_approval("u1", &approval.id, false).unwrap();
    assert_eq!(
        run.await.unwrap().unwrap_err().code(),
        Some(DeviceErrorCode::DeviceApprovalDenied)
    );

    // Allowed: the run starts on the Mac with exactly that task, and ends with the helper's event.
    let svc2 = svc.clone();
    let (a, p) = (addon.clone(), phone.clone());
    let run = tokio::spawn(async move { svc2.addon_run_task(task(&a, &p)).await });
    until("the approval is open", || async { !svc.approvals("u1").is_empty() }).await;
    svc.decide_approval("u1", &svc.approvals("u1")[0].id, true).unwrap();
    let mut seen = Vec::new();
    let req = request(&mut ws, &mut seen).await;
    assert_eq!(
        req["verb"], "addon_run",
        "the first request after the two refusals is the approved run"
    );
    assert_eq!(
        req["args"],
        json!({"name": "artemis", "task": TASK, "device": "R5CX", "max_minutes": 5})
    );
    send(
        &mut ws,
        json!({"type": "response", "id": req["id"], "ok": true, "result": {"run_id": "run-1"}}),
    )
    .await;
    for (state, extra) in [
        ("running", json!({})),
        (
            "done",
            json!({"trace_url": "https://mac.local/trace/run-1", "message": "Android 15"}),
        ),
    ] {
        let mut data = json!({"run_id": "run-1", "state": state});
        data.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        send(
            &mut ws,
            json!({"type": "event", "device": "R5CX", "name": "addon_progress", "data": data}),
        )
        .await;
    }
    let outcome = run.await.unwrap().unwrap();
    assert_eq!((outcome.run_id.as_str(), outcome.state.as_str()), ("run-1", "done"));
    assert_eq!(outcome.trace_url.as_deref(), Some("https://mac.local/trace/run-1"));
    assert_eq!(outcome.message.as_deref(), Some("Android 15"));

    // Audit, newest first: result, approval, the deny, the first refusal.
    let rows = audit(&svc, &addon).await;
    let outcomes: Vec<DeviceAuditOutcome> = rows.iter().map(|r| r.0).collect();
    assert_eq!(
        outcomes,
        vec![
            DeviceAuditOutcome::Ok,
            DeviceAuditOutcome::Approved,
            DeviceAuditOutcome::Denied,
            DeviceAuditOutcome::Refused
        ]
    );
    assert_eq!(rows[0].1["state"], "done");
    assert_eq!(rows[0].1["trace_url"], "https://mac.local/trace/run-1");
    assert!(rows[0].1["duration_ms"].as_i64().is_some());
    assert_eq!(rows[0].1["task"], TASK);
    assert_eq!(rows[3].1["reason"], "switch_off");
    assert!(svc.audit("u2", &addon, None, None).await.is_err(), "per user");
}

#[tokio::test]
async fn switching_the_phone_off_stops_a_running_task() {
    let (svc, port) = harness().await;
    let (mut ws, client) = pair_helper(&svc, port, json!([{"name": "mobilerun", "version": "1"}])).await;
    let (addon, phone) = (format!("addon:{client}:mobilerun"), format!("phone:{client}:R5CX"));
    until("cards", || async {
        svc.get("u1", &addon).await.is_ok() && svc.get("u1", &phone).await.is_ok()
    })
    .await;
    switch(&svc, &phone, true, Some(15)).await;
    switch(&svc, &addon, true, None).await;

    let svc2 = svc.clone();
    let (a, p) = (addon.clone(), phone.clone());
    let run = tokio::spawn(async move { svc2.addon_run_task(task(&a, &p)).await });
    until("the approval is open", || async { !svc.approvals("u1").is_empty() }).await;
    svc.decide_approval("u1", &svc.approvals("u1")[0].id, true).unwrap();
    let mut seen = Vec::new();
    let req = request(&mut ws, &mut seen).await;
    assert_eq!(req["verb"], "addon_run");
    send(
        &mut ws,
        json!({"type": "response", "id": req["id"], "ok": true, "result": {"run_id": "run-9"}}),
    )
    .await;

    // The owner switches the phone off mid-run: core tells the Mac to stop that run.
    switch(&svc, &phone, false, None).await;
    let req = request(&mut ws, &mut seen).await;
    assert_eq!(
        (req["verb"].as_str(), &req["args"]),
        (Some("addon_stop"), &json!({"run_id": "run-9"}))
    );
    send(
        &mut ws,
        json!({"type": "response", "id": req["id"], "ok": true, "result": {}}),
    )
    .await;
    let outcome = run.await.unwrap().unwrap();
    assert_eq!(
        (outcome.state.as_str(), outcome.message.as_deref()),
        ("stopped", Some("switched off"))
    );
    // The phone itself is told to stop as well; that frame may come before or after `addon_stop`.
    while !seen.iter().any(|f| f["type"] == "stop" && f["device"] == "R5CX") {
        seen.push(next(&mut ws).await.expect("the phone's stop frame"));
    }
    let rows = audit(&svc, &addon).await;
    assert_eq!(
        (rows[0].0, rows[0].1["state"].as_str()),
        (DeviceAuditOutcome::Error, Some("stopped"))
    );
}
