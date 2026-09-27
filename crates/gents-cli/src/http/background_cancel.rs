//! `POST /sessions/background/cancel`: an operator kill of one background tool
//! row, for clients that do not share this runtime's process (the desktop).
//! The runtime owner decides; this route only carries the session scope.

use axum::{extract::State, http::StatusCode, response::IntoResponse, response::Response, Json};
use gents::CancelBackgroundToolCallOutcome;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::http::router::RuntimeHttpState;

pub(crate) const BACKGROUND_CANCEL_PATH: &str = "/sessions/background/cancel";

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

pub(crate) async fn background_cancel_handler(
    State(state): State<RuntimeHttpState>,
    Json(body): Json<BackgroundCancelRequest>,
) -> Response {
    if body.session_id.trim().is_empty() || body.tool_call_id.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"session_id and tool_call_id are required"})),
        )
            .into_response();
    }
    let Some(runtime) = state.activation_runtime.get() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"runtime is not active"})),
        )
            .into_response();
    };
    match runtime
        .cancel_session_background_process(
            body.requester_did.as_deref(),
            &body.session_id,
            &body.tool_call_id,
        )
        .await
    {
        Ok(outcome) => (StatusCode::OK, Json(outcome_response(outcome))).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":format!("{error:#}")})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gents::CancelBackgroundToolCallOutcome as Outcome;

    /// The contract router this runtime serves, before activation.
    async fn spawn_runtime_router() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (activation_runtime, activation_observation) =
            crate::http::router::empty_activation_state();
        let router = crate::http::runtime_contract_router(
            "http://127.0.0.1:1/api/v0/graphql".to_string(),
            "cancel-test-agent".to_string(),
            "did:test:runtime".to_string(),
            "readwrite".to_string(),
            None,
            None,
            None,
            None,
            None,
            None,
            crate::http::enrollment::empty_issuer_handle(),
            crate::http::enrollment::empty_decision_service_handle(),
            activation_runtime,
            activation_observation,
            Default::default(),
            Default::default(),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        format!("http://{addr}{BACKGROUND_CANCEL_PATH}")
    }

    async fn post(body: &str) -> (StatusCode, serde_json::Value) {
        let response = reqwest::Client::new()
            .post(spawn_runtime_router().await)
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

    #[tokio::test]
    async fn refuses_before_the_runtime_is_active() {
        let (status, body) =
            post(r#"{"session_id":"s1","requester_did":null,"tool_call_id":"call-1"}"#).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body["error"].as_str().unwrap().contains("not active"));
    }

    #[tokio::test]
    async fn requires_a_session_and_a_call() {
        let (status, _) = post(r#"{"session_id":" ","tool_call_id":"call-1"}"#).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = post(r#"{"session_id":"s1"}"#).await;
        assert!(status.is_client_error());
        let (status, _) =
            post(r#"{"session_id":"s1","tool_call_id":"c","agent_did":"did:other"}"#).await;
        assert!(
            status.is_client_error(),
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
