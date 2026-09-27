// Operator kill of one native background process row. The live execution
// belongs to the runtime process, so the bridge asks that runtime through its
// local HTTP control plane (loopback only); the runtime's
// `cancel_session_background_process` owner authorizes the session scope and
// decides the outcome.

use std::sync::Arc;
use std::time::Duration;

use gents_desktop_core::client::ClientCore;

use crate::types::{BackgroundCancelResultView, DesktopCancelBackgroundProcessRequest};

/// The runtime route, served by `gents serve` beside its GraphQL.
pub const BACKGROUND_CANCEL_PATH: &str = "/sessions/background/cancel";

/// The owner waits for an observed stop before it answers; a runtime that
/// does not answer within this bound is reported, not waited on.
const BACKGROUND_CANCEL_TIMEOUT: Duration = Duration::from_secs(30);

/// The runtime's cancel route for the control-plane GraphQL URL it
/// published (`http://host:port/api/v0/graphql`).
pub fn background_cancel_url(operator_graphql: &str) -> Result<reqwest::Url, String> {
    let mut url = reqwest::Url::parse(operator_graphql.trim())
        .map_err(|error| format!("runtime GraphQL URL is not a valid URL: {error}"))?;
    url.set_path(BACKGROUND_CANCEL_PATH);
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

pub async fn cancel_background_process(
    core: &Arc<ClientCore>,
    agent_did: &str,
    request: &DesktopCancelBackgroundProcessRequest,
) -> Result<BackgroundCancelResultView, String> {
    let graphql = core
        .peer_records()
        .await
        .into_iter()
        .filter(|record| record.agent_did == agent_did)
        .find_map(|record| record.operator_graphql().map(str::to_owned))
        .ok_or_else(|| {
            format!("no local runtime hosts {agent_did}; its background work is stopped there")
        })?;
    let url = background_cancel_url(&graphql)?;
    tracing::info!(
        target: "gents_desktop::operator_control",
        agent_did,
        session_id = %request.session_id,
        tool_call_id = %request.tool_call_id,
        runtime = %url,
        "desktop background kill requested"
    );
    let result = post_background_cancel(&url, request).await;
    match &result {
        Ok(result) => tracing::info!(
            target: "gents_desktop::operator_control",
            agent_did,
            session_id = %request.session_id,
            tool_call_id = %request.tool_call_id,
            outcome = %result.outcome,
            "desktop background kill completed"
        ),
        Err(error) => tracing::warn!(
            target: "gents_desktop::operator_control",
            agent_did,
            session_id = %request.session_id,
            tool_call_id = %request.tool_call_id,
            error,
            "desktop background kill failed"
        ),
    }
    result
}

pub async fn post_background_cancel(
    url: &reqwest::Url,
    request: &DesktopCancelBackgroundProcessRequest,
) -> Result<BackgroundCancelResultView, String> {
    let client = reqwest::Client::builder()
        .timeout(BACKGROUND_CANCEL_TIMEOUT)
        .build()
        .map_err(|error| format!("building the runtime client: {error}"))?;
    let response = client
        .post(url.clone())
        .json(&serde_json::json!({
            "session_id": request.session_id,
            "requester_did": request.requester_did,
            "tool_call_id": request.tool_call_id,
        }))
        .send()
        .await
        .map_err(|error| format!("runtime unreachable at {url}: {error}"))?;
    let status = response.status();
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|error| format!("runtime returned an unreadable reply ({status}): {error}"))?;
    if !status.is_success() {
        let error = body
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("request failed");
        return Err(format!("runtime refused the kill ({status}): {error}"));
    }
    serde_json::from_value(body).map_err(|error| format!("runtime kill reply: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn targets_the_runtime_route_beside_its_graphql() {
        let url = background_cancel_url("http://127.0.0.1:9191/api/v0/graphql").unwrap();
        assert_eq!(
            url.as_str(),
            "http://127.0.0.1:9191/sessions/background/cancel"
        );
        assert!(background_cancel_url("not a url").is_err());
    }

    /// One HTTP exchange: returns what the client sent and replies `reply`.
    async fn serve_once(
        status: &str,
        reply: &str,
    ) -> (reqwest::Url, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
            reply.len()
        );
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut received = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = socket.read(&mut buf).await.unwrap();
                received.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&received);
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let length = head
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if body.len() >= length {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&received).into_owned()
        });
        let url = background_cancel_url(&format!("http://{addr}/api/v0/graphql")).unwrap();
        (url, handle)
    }

    fn request() -> DesktopCancelBackgroundProcessRequest {
        DesktopCancelBackgroundProcessRequest {
            agent_did: Some("did:test:runtime".into()),
            session_id: "sess-1".into(),
            requester_did: Some("did:test:person".into()),
            tool_call_id: "call-1".into(),
        }
    }

    #[tokio::test]
    async fn posts_the_session_scope_and_reads_the_owner_outcome() {
        let (url, sent) = serve_once("200 OK", r#"{"outcome":"cancelled"}"#).await;
        let result = post_background_cancel(&url, &request()).await.unwrap();
        assert_eq!(result.outcome, "cancelled");
        assert_eq!(result.state, None);
        let sent = sent.await.unwrap();
        assert!(sent.starts_with("POST /sessions/background/cancel "));
        let body: serde_json::Value =
            serde_json::from_str(sent.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "session_id": "sess-1",
                "requester_did": "did:test:person",
                "tool_call_id": "call-1",
            }),
            "the agent is the runtime's own principal and is never sent"
        );
    }

    #[tokio::test]
    async fn reports_a_terminal_row_with_its_state() {
        let (url, _) = serve_once(
            "200 OK",
            r#"{"outcome":"already_terminal","state":"completed"}"#,
        )
        .await;
        let result = post_background_cancel(&url, &request()).await.unwrap();
        assert_eq!(result.outcome, "already_terminal");
        assert_eq!(result.state.as_deref(), Some("completed"));
    }

    #[tokio::test]
    async fn surfaces_a_runtime_refusal() {
        let (url, _) = serve_once(
            "503 Service Unavailable",
            r#"{"error":"runtime is not active"}"#,
        )
        .await;
        let error = post_background_cancel(&url, &request()).await.unwrap_err();
        assert!(error.contains("runtime is not active"), "{error}");
    }
}
