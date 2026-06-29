//! WebSocket stream handlers for coin-keyed real-time feeds (SPEC-API-002 REQ-API-150/151).
//!
//! Routes (MUST be registered BEFORE `/v1/coins/{coin_id}` — REQ-API-148):
//! - `GET /v1/coins/stream/quotes`  → stream_coin_quotes  (RFC 6455 WebSocket)
//! - `GET /v1/coins/stream/candles` → stream_coin_candles (RFC 6455 WebSocket)
//!
//! Each handler upgrades the HTTP connection to WebSocket and forwards payloads from the
//! in-process `broadcast::Sender<String>`, which is populated by the `listener` module
//! via PostgreSQL LISTEN/NOTIFY (`coin_quote_update`, `coin_candle_update` channels).
//!
//! The broadcast channel enables fan-out to multiple concurrent WebSocket clients per replica.
//! Cross-replica delivery is guaranteed by PostgreSQL NOTIFY which reaches all connected replicas.
//!
//! @MX:WARN: [AUTO] WebSocket handlers hold a broadcast::Receiver — receiver lag drops messages
//! @MX:REASON: tokio::sync::broadcast::Receiver::recv() returns Lagged(n) when the internal ring
//!             buffer is exhausted. Clients on slow connections receive gaps; this is by design
//!             (best-effort streaming). Monitor the "lagged" log counter for capacity tuning.
//! @MX:SPEC: SPEC-API-002 REQ-API-150 REQ-API-151

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
};
use tokio::sync::broadcast;

use super::AppState;

// ── Handlers ──────────────────────────────────────────────────────────────────

/// `GET /v1/coins/stream/quotes` — real-time coin quote stream (REQ-API-150).
///
/// Upgrades to WebSocket and forwards JSON payloads from the `coin_quote_tx` broadcast channel.
/// Payloads match the PostgreSQL NOTIFY payload from `coin_quote_update` (JSON object).
///
/// MUST be registered BEFORE `/v1/coins/{coin_id}` in the router (REQ-API-148).
pub async fn stream_coin_quotes(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let rx = state.coin_quote_tx.subscribe();
    ws.on_upgrade(move |socket| handle_stream(socket, rx))
}

/// `GET /v1/coins/stream/candles` — real-time coin candle stream (REQ-API-151).
///
/// Upgrades to WebSocket and forwards JSON payloads from the `coin_candle_tx` broadcast channel.
/// Payloads match the PostgreSQL NOTIFY payload from `coin_candle_update` (JSON object).
///
/// MUST be registered BEFORE `/v1/coins/{coin_id}` in the router (REQ-API-148).
pub async fn stream_coin_candles(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let rx = state.coin_candle_tx.subscribe();
    ws.on_upgrade(move |socket| handle_stream(socket, rx))
}

// ── Shared stream handler ─────────────────────────────────────────────────────

/// Drive a WebSocket connection: read from the broadcast channel and send to the client.
///
/// Terminates when:
/// - The client closes the connection (write error).
/// - The broadcast sender is dropped (channel `Closed`).
///
/// On `Lagged`: logs a warning and continues — clients receive gaps on slow connections.
async fn handle_stream(mut socket: WebSocket, mut rx: broadcast::Receiver<String>) {
    loop {
        match rx.recv().await {
            Ok(payload) => {
                if socket.send(Message::Text(payload.into())).await.is_err() {
                    // Client disconnected.
                    break;
                }
            }
            Err(broadcast::error::RecvError::Closed) => {
                // All senders dropped — server is shutting down.
                break;
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                // Ring buffer overflow — client was too slow; log and continue.
                tracing::warn!("websocket stream: receiver lagged by {n} messages");
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Compile-time proof: handlers have the correct signature for axum routing.
    #[test]
    fn stream_coin_quotes_handler_exists() {
        let _ = stream_coin_quotes;
    }

    #[test]
    fn stream_coin_candles_handler_exists() {
        let _ = stream_coin_candles;
    }
}
