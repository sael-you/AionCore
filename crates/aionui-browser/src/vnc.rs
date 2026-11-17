//! The Camoufox panel's view (TASK-136 K6, UI card U4):
//! `WS /api/browser/instances/{n}/vnc`, behind the same session auth and
//! trusted-Origin layers as `/api/browser/ws` (mounted in `browser_routes`).
//!
//! Binary frames both ways are raw RFB 3.8 bytes between the panel's noVNC
//! client and the instance's x11vnc, which listens only on a Unix socket in
//! the instance HOME (no TCP port; security type None — only AionCore can
//! reach the socket). Connecting starts the instance (like the panel's
//! `Start` for Chrome). Close codes: 4409 not a Camoufox instance, 4503 the
//! start failed (reason = error code).

use std::sync::Arc;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::error::BrowserError;
use crate::juggler::JugglerInstance;

pub const CLOSE_NOT_CAMOUFOX: u16 = 4409;
pub const CLOSE_START_FAILED: u16 = 4503;
/// Largest RFB chunk sent in one frame.
const CHUNK: usize = 64 * 1024;

/// Close the socket with `code` and a short reason.
pub async fn close_with(mut socket: WebSocket, code: u16, reason: &str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.to_owned().into(),
        })))
        .await;
}

/// The close code and reason for a start failure.
pub fn start_failure(e: &BrowserError) -> (u16, String) {
    let reason = e
        .code()
        .map(crate::routes::code_str)
        .unwrap_or("BROWSER_START_FAILED")
        .to_owned();
    (CLOSE_START_FAILED, reason)
}

/// Relay RFB bytes until either side closes.
pub async fn relay(socket: WebSocket, inst: Arc<JugglerInstance>) {
    let stream = match tokio::net::UnixStream::connect(inst.vnc_socket()).await {
        Ok(s) => s,
        Err(_) => {
            close_with(socket, CLOSE_START_FAILED, "BROWSER_START_FAILED").await;
            return;
        }
    };
    tracing::info!(user_id = %inst.user_id, instance = inst.instance, "camoufox: panel view opened");
    let (mut rd, mut wr) = stream.into_split();
    let (mut tx, mut rx) = socket.split();
    let to_panel = {
        let inst = inst.clone();
        async move {
            let mut buf = vec![0u8; CHUNK];
            loop {
                match rd.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        inst.touch();
                        if tx.send(Message::Binary(buf[..n].to_vec().into())).await.is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = tx.close().await;
        }
    };
    let from_panel = async move {
        while let Some(Ok(msg)) = rx.next().await {
            match msg {
                Message::Binary(bytes) => {
                    if wr.write_all(&bytes).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                // RFB is binary only; text frames are ignored.
                _ => {}
            }
        }
        let _ = wr.shutdown().await;
    };
    tokio::select! {
        _ = to_panel => {}
        _ = from_panel => {}
    }
    tracing::info!(user_id = %inst.user_id, instance = inst.instance, "camoufox: panel view closed");
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_api_types::BrowserErrorCode;

    #[test]
    fn start_failures_close_with_4503_and_the_code() {
        let (code, reason) = start_failure(&BrowserError::coded(BrowserErrorCode::BrowserUnavailable, "x"));
        assert_eq!(code, 4503);
        assert_eq!(reason, "BROWSER_UNAVAILABLE");
        assert_eq!(
            start_failure(&BrowserError::Internal("x".into())).1,
            "BROWSER_START_FAILED"
        );
    }
}
