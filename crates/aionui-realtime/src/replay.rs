//! Per-user frame sequencing and a bounded replay ring (TASK-43, option C).
//!
//! Every frame pushed to a user's stream (`broadcast_to_user`, and
//! `broadcast_all` once per user) gets the user's next `seq` and is kept in an
//! in-memory ring as its serialized text. A client that reconnects (or sees a
//! gap) sends `realtime.resume {epoch, last_seq}`; the server answers one
//! `realtime.replay` with every missed frame, or `realtime.resyncRequired` when
//! the frames are gone (ring trimmed) or the process restarted (`epoch`).
//!
//! Ordering: stamping, recording and delivering a live frame, registering a
//! connection (+ its `realtime.hello`) and building a replay all run under the
//! user's stream lock, so a replay never interleaves with live frames.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use serde_json::Value;

/// Bounds of one user's replay ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayConfig {
    pub max_frames: usize,
    pub max_age: Duration,
    /// Serialized bytes kept per user (memory bound).
    pub max_bytes: usize,
    /// A stream with no connection and no frame for this long is dropped.
    pub idle_expiry: Duration,
}

impl Default for ReplayConfig {
    fn default() -> Self {
        Self {
            max_frames: 2_000,
            max_age: Duration::from_secs(5 * 60),
            max_bytes: 4 * 1024 * 1024,
            idle_expiry: Duration::from_secs(5 * 60),
        }
    }
}

struct Entry {
    seq: u64,
    at: Instant,
    text: String,
}

/// One user's stream: the last assigned seq and the frames still replayable.
pub(crate) struct UserStream {
    last_seq: u64,
    ring: VecDeque<Entry>,
    bytes: usize,
    last_active: Instant,
}

impl UserStream {
    fn new(now: Instant) -> Self {
        Self {
            last_seq: 0,
            ring: VecDeque::new(),
            bytes: 0,
            last_active: now,
        }
    }

    pub(crate) fn last_seq(&self) -> u64 {
        self.last_seq
    }

    fn trim(&mut self, cfg: &ReplayConfig, now: Instant) {
        while let Some(front) = self.ring.front() {
            let too_many = self.ring.len() > cfg.max_frames;
            let too_big = self.bytes > cfg.max_bytes;
            let too_old = now.duration_since(front.at) > cfg.max_age;
            if !(too_many || too_big || too_old) {
                break;
            }
            let gone = self.ring.pop_front().expect("front exists");
            self.bytes -= gone.text.len();
        }
    }
}

/// What `realtime.resume` gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeOutcome {
    /// Every frame with `last_seq < seq ≤ to_seq`, serialized, in order.
    Replay {
        from_seq: u64,
        to_seq: u64,
        frames: Vec<String>,
    },
    Resync {
        seq: u64,
        reason: &'static str,
    },
}

/// The per-user sequencing state of one WebSocket manager.
pub struct ReplayLog {
    epoch: String,
    cfg: ReplayConfig,
    streams: DashMap<String, Mutex<UserStream>>,
}

impl ReplayLog {
    pub fn new(cfg: ReplayConfig) -> Self {
        // Unique per process: a restart (deploy) changes it, so a client's old
        // seq is never matched against a new counter.
        let epoch = format!(
            "{:x}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        Self {
            epoch,
            cfg,
            streams: DashMap::new(),
        }
    }

    pub fn epoch(&self) -> &str {
        &self.epoch
    }

    pub fn config(&self) -> ReplayConfig {
        self.cfg
    }

    /// Run `f` under the user's stream lock (created on first use).
    pub(crate) fn with_stream<R>(&self, user_id: &str, f: impl FnOnce(&mut UserStream) -> R) -> R {
        let entry = self
            .streams
            .entry(user_id.to_owned())
            .or_insert_with(|| Mutex::new(UserStream::new(Instant::now())));
        let mut stream = entry.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut stream)
    }

    /// Users with a stream (they may have no connection right now).
    pub(crate) fn users(&self) -> Vec<String> {
        self.streams.iter().map(|e| e.key().clone()).collect()
    }

    /// Assign the next seq, serialize `{name, data, seq}` and record it.
    pub(crate) fn stamp(&self, stream: &mut UserStream, name: &str, data_json: &str) -> String {
        let now = Instant::now();
        stream.last_seq += 1;
        let seq = stream.last_seq;
        let name_json = serde_json::to_string(name).unwrap_or_else(|_| "\"\"".into());
        let text = format!("{{\"name\":{name_json},\"data\":{data_json},\"seq\":{seq}}}");
        stream.bytes += text.len();
        stream.ring.push_back(Entry {
            seq,
            at: now,
            text: text.clone(),
        });
        stream.last_active = now;
        stream.trim(&self.cfg, now);
        text
    }

    /// The answer to `realtime.resume {epoch, last_seq}`.
    pub(crate) fn resume(&self, stream: &mut UserStream, epoch: &str, last_seq: u64) -> ResumeOutcome {
        let now = Instant::now();
        stream.trim(&self.cfg, now);
        stream.last_active = now;
        let current = stream.last_seq;
        if epoch != self.epoch || last_seq > current {
            return ResumeOutcome::Resync {
                seq: current,
                reason: "epoch_changed",
            };
        }
        if last_seq == current {
            return ResumeOutcome::Replay {
                from_seq: last_seq + 1,
                to_seq: current,
                frames: Vec::new(),
            };
        }
        // The first missed frame must still be in the ring.
        match stream.ring.front() {
            Some(front) if front.seq <= last_seq + 1 => ResumeOutcome::Replay {
                from_seq: last_seq + 1,
                to_seq: current,
                frames: stream
                    .ring
                    .iter()
                    .filter(|e| e.seq > last_seq)
                    .map(|e| e.text.clone())
                    .collect(),
            },
            _ => ResumeOutcome::Resync {
                seq: current,
                reason: "too_old",
            },
        }
    }

    /// Drop streams of users without a connection that went quiet.
    pub(crate) fn sweep(&self, connected: impl Fn(&str) -> bool) {
        let now = Instant::now();
        let expiry = self.cfg.idle_expiry;
        self.streams.retain(|user, stream| {
            if connected(user) {
                return true;
            }
            let stream = stream.get_mut().unwrap_or_else(|poisoned| poisoned.into_inner());
            now.duration_since(stream.last_active) <= expiry
        });
    }

    /// Bytes held across all users (tests, diagnostics).
    pub fn held_bytes(&self) -> usize {
        self.streams
            .iter()
            .map(|e| e.value().lock().map(|s| s.bytes).unwrap_or(0))
            .sum()
    }

    /// Frames held for one user (tests, diagnostics).
    pub fn held_frames(&self, user_id: &str) -> usize {
        self.streams
            .get(user_id)
            .and_then(|s| s.lock().ok().map(|s| s.ring.len()))
            .unwrap_or(0)
    }
}

/// `realtime.hello {epoch, seq}`: the first frame of every connection.
pub(crate) fn hello_text(epoch: &str, seq: u64) -> String {
    serde_json::json!({"name": "realtime.hello", "data": {"epoch": epoch, "seq": seq}}).to_string()
}

/// The reply to `realtime.resume`, frames embedded verbatim (no re-parse).
pub(crate) fn outcome_text(epoch: &str, outcome: &ResumeOutcome) -> String {
    match outcome {
        ResumeOutcome::Replay {
            from_seq,
            to_seq,
            frames,
        } => {
            let epoch_json = Value::String(epoch.to_owned()).to_string();
            format!(
                "{{\"name\":\"realtime.replay\",\"data\":{{\"epoch\":{epoch_json},\"from_seq\":{from_seq},\"to_seq\":{to_seq},\"frames\":[{}]}}}}",
                frames.join(",")
            )
        }
        ResumeOutcome::Resync { seq, reason } => serde_json::json!({
            "name": "realtime.resyncRequired",
            "data": {"epoch": epoch, "seq": seq, "reason": reason},
        })
        .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(cfg: ReplayConfig) -> ReplayLog {
        ReplayLog::new(cfg)
    }

    fn push(log: &ReplayLog, user: &str, n: usize) {
        for i in 0..n {
            log.with_stream(user, |s| log.stamp(s, "message.stream", &format!("{{\"i\":{i}}}")));
        }
    }

    #[test]
    fn seq_is_per_user_and_strictly_increasing() {
        let log = log(ReplayConfig::default());
        let a1 = log.with_stream("a", |s| log.stamp(s, "x", "{}"));
        let a2 = log.with_stream("a", |s| log.stamp(s, "x", "{}"));
        let b1 = log.with_stream("b", |s| log.stamp(s, "x", "{}"));
        let seq = |t: &str| serde_json::from_str::<Value>(t).unwrap()["seq"].as_u64().unwrap();
        assert_eq!((seq(&a1), seq(&a2), seq(&b1)), (1, 2, 1));
        let v: Value = serde_json::from_str(&a2).unwrap();
        assert_eq!(v["name"], "x");
        assert!(v["data"].is_object());
    }

    #[test]
    fn a_gap_is_replayed_in_order() {
        let log = log(ReplayConfig::default());
        push(&log, "a", 5);
        let out = log.with_stream("a", |s| log.resume(s, log.epoch(), 2));
        let ResumeOutcome::Replay {
            from_seq,
            to_seq,
            frames,
        } = out
        else {
            panic!("replay expected: {out:?}");
        };
        assert_eq!((from_seq, to_seq), (3, 5));
        let seqs: Vec<u64> = frames
            .iter()
            .map(|t| serde_json::from_str::<Value>(t).unwrap()["seq"].as_u64().unwrap())
            .collect();
        assert_eq!(seqs, vec![3, 4, 5]);
        // Nothing missed: an empty replay.
        let out = log.with_stream("a", |s| log.resume(s, log.epoch(), 5));
        assert_eq!(
            out,
            ResumeOutcome::Replay {
                from_seq: 6,
                to_seq: 5,
                frames: vec![]
            }
        );
    }

    #[test]
    fn frames_that_fell_out_of_the_ring_need_a_resync() {
        let log = log(ReplayConfig {
            max_frames: 3,
            ..ReplayConfig::default()
        });
        push(&log, "a", 10);
        assert_eq!(log.held_frames("a"), 3);
        let out = log.with_stream("a", |s| log.resume(s, log.epoch(), 2));
        assert_eq!(
            out,
            ResumeOutcome::Resync {
                seq: 10,
                reason: "too_old"
            }
        );
        // The oldest kept frame (8) still replays from last_seq 7.
        assert!(matches!(
            log.with_stream("a", |s| log.resume(s, log.epoch(), 7)),
            ResumeOutcome::Replay { from_seq: 8, .. }
        ));
    }

    #[test]
    fn a_restart_or_a_foreign_epoch_needs_a_resync() {
        let log = log(ReplayConfig::default());
        push(&log, "a", 3);
        assert_eq!(
            log.with_stream("a", |s| log.resume(s, "old-epoch", 2)),
            ResumeOutcome::Resync {
                seq: 3,
                reason: "epoch_changed"
            }
        );
        // A seq this counter never issued (client from before a restart).
        assert!(matches!(
            log.with_stream("a", |s| log.resume(s, log.epoch(), 99)),
            ResumeOutcome::Resync {
                reason: "epoch_changed",
                ..
            }
        ));
    }

    #[test]
    fn memory_is_bounded_by_bytes_and_age() {
        let log = log(ReplayConfig {
            max_bytes: 1_000,
            ..ReplayConfig::default()
        });
        push(&log, "a", 500);
        assert!(log.held_bytes() <= 1_000, "{}", log.held_bytes());
        assert!(log.held_frames("a") > 0);

        let log = super::ReplayLog::new(ReplayConfig {
            max_age: Duration::ZERO,
            ..ReplayConfig::default()
        });
        push(&log, "a", 3);
        std::thread::sleep(Duration::from_millis(5));
        assert!(matches!(
            log.with_stream("a", |s| log.resume(s, log.epoch(), 1)),
            ResumeOutcome::Resync { reason: "too_old", .. }
        ));
    }

    #[test]
    fn idle_streams_without_a_connection_are_swept() {
        let log = log(ReplayConfig {
            idle_expiry: Duration::ZERO,
            ..ReplayConfig::default()
        });
        push(&log, "gone", 2);
        push(&log, "here", 2);
        std::thread::sleep(Duration::from_millis(5));
        log.sweep(|user| user == "here");
        assert_eq!(log.held_frames("gone"), 0);
        assert_eq!(log.held_frames("here"), 2);
    }

    #[test]
    fn the_replay_frame_embeds_the_frames_verbatim() {
        let text = outcome_text(
            "e1",
            &ResumeOutcome::Replay {
                from_seq: 3,
                to_seq: 4,
                frames: vec![
                    r#"{"name":"a","data":{},"seq":3}"#.into(),
                    r#"{"name":"b","data":{},"seq":4}"#.into(),
                ],
            },
        );
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["name"], "realtime.replay");
        assert_eq!(v["data"]["epoch"], "e1");
        assert_eq!(v["data"]["frames"][1]["name"], "b");
        let resync = outcome_text(
            "e1",
            &ResumeOutcome::Resync {
                seq: 9,
                reason: "too_old",
            },
        );
        let v: Value = serde_json::from_str(&resync).unwrap();
        assert_eq!(v["name"], "realtime.resyncRequired");
        assert_eq!(v["data"]["reason"], "too_old");
        assert_eq!(v["data"]["seq"], 9);
    }
}
