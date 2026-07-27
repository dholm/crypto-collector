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
//! @MX:WARN: [AUTO] handle_stream is a bidirectional select! loop — it MUST poll socket.recv()
//!           so a client Close/disconnect terminates the task, and MUST keep the broadcast lag
//!           path (Lagged is by design). Reverting to a send-only loop leaks the task until the
//!           next broadcast send fails, and drops the ping-based dead-peer detection (F-36).
//! @MX:REASON: the loop select!s over rx.recv() (broadcast payload → client), socket.recv()
//!             (client frame → terminate on Close/None), and a periodic ping timer (dead-peer
//!             detection). broadcast::Receiver::recv() Lagged(n) is best-effort streaming, not an
//!             error. Per-coin subscription filtering is explicitly deferred (D8).
//! @MX:SPEC: SPEC-API-002 REQ-API-150 REQ-API-151 SPEC-API-005 REQ-API-413

use std::time::Duration;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
};
use tokio::sync::broadcast;

use super::AppState;

/// Interval between server-initiated WebSocket pings (dead-peer detection).
const PING_INTERVAL: Duration = Duration::from_secs(30);

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

/// Drive a WebSocket connection with a bidirectional `select!` loop (F-36, REQ-API-413).
///
/// Terminates when:
/// - The client sends a `Close` frame, or the socket returns `None`/an error (disconnect).
/// - A send to the client fails (client gone).
/// - The broadcast sender is dropped (channel `Closed` — server shutting down).
///
/// The loop also arms a periodic ping timer so a silent/dead peer (one that never sends a Close)
/// is detected. On broadcast `Lagged`: logs a warning and continues — clients on slow connections
/// receive gaps (best-effort streaming). Per-coin subscription filtering is deferred (D8): any
/// non-Close client frame is ignored.
async fn handle_stream(mut socket: WebSocket, mut rx: broadcast::Receiver<String>) {
    let mut ping_timer = tokio::time::interval(PING_INTERVAL);
    // Skip the immediate first tick so the first ping fires one interval from now, not instantly.
    ping_timer.tick().await;

    loop {
        tokio::select! {
            // Broadcast payload → forward to the client.
            payload = rx.recv() => match payload {
                Ok(payload) => {
                    if socket.send(Message::Text(payload.into())).await.is_err() {
                        break; // client gone
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break, // all senders dropped
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("websocket stream: receiver lagged by {n} messages");
                }
            },

            // Client frame → terminate on Close/None/error; ignore other frames (D8).
            client_frame = socket.recv() => match client_frame {
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {} // subscription-control frames not yet handled (D8)
                Some(Err(_)) => break, // socket error → terminate
            },

            // Periodic ping → detect dead peers that never send a Close.
            _ = ping_timer.tick() => {
                if socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break; // client gone
                }
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

    // ── SPEC-API-005 M7 (F-36, REQ-API-413): bidirectional read loop ─────────────

    // Poll a predicate with a generous bounded timeout (deterministic, non-flaky).
    async fn wait_for<F: Fn() -> bool>(pred: F) -> bool {
        for _ in 0..500 {
            if pred() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        pred()
    }

    fn ws_test_state(coin_quote_tx: broadcast::Sender<String>) -> AppState {
        let (coin_candle_tx, _) = broadcast::channel(16);
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/crypto_collector_test")
            .expect("lazy pool");
        AppState {
            pool,
            chain: std::sync::Arc::new(vec![]),
            search_provider: "coingecko".into(),
            coingecko_base_url: "https://api.coingecko.com".into(),
            http_client: reqwest::Client::new(),
            coin_quote_tx,
            coin_candle_tx,
        }
    }

    // AC-API-413: a client Close frame terminates the server-side stream task. Termination is
    // observed via the broadcast receiver_count: handle_stream subscribes on upgrade and drops
    // its Receiver when it breaks, so the count returns to baseline after the Close. The test
    // also proves the rx→client path still works under the new select! loop.
    #[tokio::test]
    async fn ws_client_close_terminates_stream_task() {
        use axum_test::TestServer;

        let (coin_quote_tx, _keep) = broadcast::channel::<String>(16);
        let state = ws_test_state(coin_quote_tx.clone());
        let server = TestServer::builder()
            .http_transport()
            .build(crate::api::build_api_router(state));

        let baseline = coin_quote_tx.receiver_count();

        let mut ws = server
            .get_websocket("/v1/coins/stream/quotes")
            .await
            .into_websocket()
            .await;

        // handle_stream subscribed during the upgrade → one more receiver than baseline.
        assert!(
            wait_for(|| coin_quote_tx.receiver_count() == baseline + 1).await,
            "handle_stream must subscribe to the broadcast channel on upgrade"
        );

        // The rx→client path works under the select! loop: a broadcast reaches the client.
        coin_quote_tx
            .send(r#"{"coin_id":"bitcoin","price":"1"}"#.to_string())
            .expect("broadcast send");
        let received = ws.receive_text().await;
        assert!(
            received.contains("bitcoin"),
            "client must receive the broadcast payload; got {received}"
        );

        // Client closes → select! reacts to the Close frame → handle_stream breaks → drops its
        // Receiver, so the count returns to baseline (the task terminated).
        ws.close().await;
        assert!(
            wait_for(|| coin_quote_tx.receiver_count() == baseline).await,
            "a client Close frame must terminate the server-side stream task (REQ-API-413)"
        );
    }
}
