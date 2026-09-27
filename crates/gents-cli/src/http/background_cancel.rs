//! `POST /sessions/background/cancel`: an operator kill of one native
//! background process row, for a client that does not share this runtime's
//! process (the desktop). The runtime owner decides; this route carries the
//! session scope.
//!
//! The body's `requester_did` is not authenticated, so the route answers
//! only loopback peers: a client on this host already holds the runtime's
//! store. DID-authenticated operator control is a follow-up.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::{
    extract::{ConnectInfo, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use gents::CancelBackgroundToolCallOutcome;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::OnceCell;

pub(crate) const BACKGROUND_CANCEL_PATH: &str = "/sessions/background/cancel";

const MAX_BODY_BYTES: usize = 16 * 1024;

#[derive(Clone)]
pub(crate) struct BackgroundCancelState {
    runtime: Arc<OnceCell<gents::Gents>>,
    /// The address the embedded HTTP server listens on.
    bind: IpAddr,
}

/// The kill route, served beside the runtime contract routes on `bind`.
pub(crate) fn background_cancel_router(
    runtime: Arc<OnceCell<gents::Gents>>,
    bind: IpAddr,
) -> Router {
    Router::new()
        .route(BACKGROUND_CANCEL_PATH, post(background_cancel_handler))
        .with_state(BackgroundCancelState { runtime, bind })
}

/// Whether a request may use operator control. A known peer must be
/// loopback. Without connection info (the embedded server does not record
/// it) only a loopback-bound listener qualifies, since it accepts nothing
/// else; an unspecified or routable bind is refused.
pub(crate) fn peer_is_local(bind: IpAddr, peer: Option<SocketAddr>) -> bool {
    match peer {
        Some(peer) => peer.ip().to_canonical().is_loopback(),
        None => bind.to_canonical().is_loopback(),
    }
}

/// The session scope and logical call id of the row to stop. The agent is
/// always this runtime's principal.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BackgroundCancelRequest {
    session_id: String,
    #[serde(default)]
    requester_did: Option<String>,
    tool_call_id: String,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BackgroundCancelResponse {
    pub(crate) outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) state: Option<String>,
}

pub(crate) fn outcome_response(
    outcome: CancelBackgroundToolCallOutcome,
) -> BackgroundCancelResponse {
    BackgroundCancelResponse {
        outcome: outcome.label().to_string(),
        state: outcome.terminal_state().map(str::to_owned),
    }
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

pub(crate) async fn background_cancel_handler(
    State(state): State<BackgroundCancelState>,
    request: Request,
) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0);
    if !peer_is_local(state.bind, peer) {
        tracing::warn!(
            target: "gents::operator_control",
            bind = %state.bind,
            peer = ?peer,
            "refused a background kill from a non-loopback peer"
        );
        return error(
            StatusCode::FORBIDDEN,
            "background kill is only served to loopback clients",
        );
    }
    let body = match axum::body::to_bytes(request.into_body(), MAX_BODY_BYTES).await {
        Ok(body) => body,
        Err(err) => return error(StatusCode::BAD_REQUEST, format!("reading body: {err}")),
    };
    let body: BackgroundCancelRequest = match serde_json::from_slice(&body) {
        Ok(body) => body,
        Err(err) => return error(StatusCode::BAD_REQUEST, format!("decoding body: {err}")),
    };
    if body.session_id.trim().is_empty() || body.tool_call_id.trim().is_empty() {
        return error(
            StatusCode::BAD_REQUEST,
            "session_id and tool_call_id are required",
        );
    }
    let Some(runtime) = state.runtime.get() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "runtime is not active");
    };
    tracing::info!(
        target: "gents::operator_control",
        session_id = %body.session_id,
        requester_did = body.requester_did.as_deref().unwrap_or(""),
        tool_call_id = %body.tool_call_id,
        "operator background kill requested"
    );
    match runtime
        .cancel_session_background_process(
            body.requester_did.as_deref(),
            &body.session_id,
            &body.tool_call_id,
        )
        .await
    {
        Ok(outcome) => {
            tracing::info!(
                target: "gents::operator_control",
                session_id = %body.session_id,
                tool_call_id = %body.tool_call_id,
                outcome = outcome.label(),
                "operator background kill completed"
            );
            (StatusCode::OK, Json(outcome_response(outcome))).into_response()
        }
        Err(err) => {
            tracing::warn!(
                target: "gents::operator_control",
                session_id = %body.session_id,
                tool_call_id = %body.tool_call_id,
                error = %format!("{err:#}"),
                "operator background kill failed"
            );
            error(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;
    use gents::CancelBackgroundToolCallOutcome as Outcome;

    /// Serve the kill route on loopback. `peer` stands in for the connection
    /// info a routable listener would record; `None` serves it as the
    /// embedded server does, with no connection info.
    async fn serve(bind: IpAddr, peer: Option<SocketAddr>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut router = background_cancel_router(Arc::new(OnceCell::new()), bind);
        if let Some(peer) = peer {
            router = router.layer(axum::Extension(ConnectInfo(peer)));
        }
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        format!("http://{addr}{BACKGROUND_CANCEL_PATH}")
    }

    async fn post_to(url: String, body: &str) -> (StatusCode, serde_json::Value) {
        let response = reqwest::Client::new()
            .post(url)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .unwrap();
        let status = StatusCode::from_u16(response.status().as_u16()).unwrap();
        (
            status,
            response.json().await.unwrap_or(serde_json::Value::Null),
        )
    }

    const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
    const BODY: &str = r#"{"session_id":"s1","requester_did":null,"tool_call_id":"call-1"}"#;

    #[test]
    fn only_loopback_peers_are_local() {
        let peer = |ip: [u8; 4]| Some(SocketAddr::from((ip, 4000)));
        assert!(peer_is_local(LOOPBACK, peer([127, 0, 0, 1])));
        assert!(!peer_is_local(LOOPBACK, peer([10, 0, 0, 5])));
        assert!(!peer_is_local(LOOPBACK, peer([192, 168, 1, 2])));
        let mapped: SocketAddr = "[::ffff:127.0.0.1]:4000".parse().unwrap();
        assert!(peer_is_local(LOOPBACK, Some(mapped)));
        let routable: SocketAddr = "[2001:db8::1]:4000".parse().unwrap();
        assert!(!peer_is_local(LOOPBACK, Some(routable)));
        // no connection info: only a loopback-bound listener qualifies
        assert!(peer_is_local(LOOPBACK, None));
        assert!(!peer_is_local(IpAddr::V4(Ipv4Addr::UNSPECIFIED), None));
        assert!(!peer_is_local(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), None));
    }

    #[tokio::test]
    async fn refuses_a_non_loopback_peer() {
        let url = serve(LOOPBACK, Some(SocketAddr::from(([10, 0, 0, 5], 4000)))).await;
        let (status, body) = post_to(url, BODY).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body["error"].as_str().unwrap().contains("loopback"));
    }

    #[tokio::test]
    async fn refuses_every_client_of_a_routable_bind_without_connection_info() {
        let url = serve(IpAddr::V4(Ipv4Addr::UNSPECIFIED), None).await;
        let (status, _) = post_to(url, BODY).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn refuses_before_the_runtime_is_active() {
        let url = serve(LOOPBACK, Some(SocketAddr::from(([127, 0, 0, 1], 4000)))).await;
        let (status, body) = post_to(url, BODY).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body["error"].as_str().unwrap().contains("not active"));
    }

    #[tokio::test]
    async fn requires_a_session_and_a_call() {
        let (status, _) = post_to(
            serve(LOOPBACK, None).await,
            r#"{"session_id":" ","tool_call_id":"call-1"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = post_to(serve(LOOPBACK, None).await, r#"{"session_id":"s1"}"#).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = post_to(
            serve(LOOPBACK, None).await,
            r#"{"session_id":"s1","tool_call_id":"c","agent_did":"did:other"}"#,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "the agent is this runtime's principal, never the caller's choice"
        );
    }

    #[test]
    fn reports_every_owner_outcome() {
        for (outcome, expected, state) in [
            (
                Outcome::Cancelled {
                    live_execution_cancelled: true,
                },
                "cancelled",
                None,
            ),
            (Outcome::Lost, "lost", None),
            (Outcome::Unverified, "unverified", None),
            (
                Outcome::AlreadyTerminal {
                    state: "completed".to_string(),
                },
                "already_terminal",
                Some("completed"),
            ),
            (Outcome::NotBackground, "not_background", None),
            (Outcome::NotFound, "not_found", None),
        ] {
            let response = outcome_response(outcome);
            assert_eq!(response.outcome, expected);
            assert_eq!(response.state.as_deref(), state);
        }
    }
}
