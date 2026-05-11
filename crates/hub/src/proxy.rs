use crate::audit::AuditEvent;
use crate::{auth, AppState};
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

const MAX_BODY: usize = 32 * 1024 * 1024;

pub async fn anthropic_messages(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    req: Request<Body>,
) -> Response {
    let account = match auth::authenticate(&state.config.accounts, &headers) {
        Ok(a) => a,
        Err(reason) => {
            state.audit.write(AuditEvent {
                provider: Some("anthropic".into()),
                status: Some(401),
                reason: Some(reason.into()),
                ..AuditEvent::new("auth_denied")
            });
            return (StatusCode::UNAUTHORIZED, reason).into_response();
        }
    };

    let allowed = account
        .allowed_providers
        .iter()
        .any(|p| p == "anthropic" || p == "*");
    if !allowed {
        state.audit.write(AuditEvent {
            account: Some(account.name.clone()),
            provider: Some("anthropic".into()),
            status: Some(403),
            reason: Some("provider not allowed".into()),
            ..AuditEvent::new("auth_denied")
        });
        return (StatusCode::FORBIDDEN, "provider not allowed").into_response();
    }

    let body_bytes = match axum::body::to_bytes(req.into_body(), MAX_BODY).await {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("body: {}", e)).into_response(),
    };

    let parsed: Option<serde_json::Value> = serde_json::from_slice(&body_bytes).ok();
    let model = parsed
        .as_ref()
        .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(String::from));
    let stream = parsed
        .as_ref()
        .and_then(|v| v.get("stream").and_then(|s| s.as_bool()))
        .unwrap_or(false);

    let url = format!(
        "{}/v1/messages",
        state.config.anthropic.upstream.trim_end_matches('/')
    );
    let mut builder = state
        .http
        .post(&url)
        .header("x-api-key", &state.config.anthropic.api_key)
        .header("content-type", "application/json");

    for k in ["anthropic-version", "anthropic-beta"] {
        if let Some(v) = headers.get(k) {
            builder = builder.header(k, v);
        }
    }
    if headers.get("anthropic-version").is_none() {
        builder = builder.header("anthropic-version", "2023-06-01");
    }

    let upstream = match builder.body(body_bytes.to_vec()).send().await {
        Ok(r) => r,
        Err(e) => {
            state.audit.write(AuditEvent {
                account: Some(account.name.clone()),
                provider: Some("anthropic".into()),
                model: model.clone(),
                status: Some(502),
                stream: Some(stream),
                reason: Some(format!("upstream: {}", e)),
                ..AuditEvent::new("messages_request")
            });
            return (StatusCode::BAD_GATEWAY, format!("upstream: {}", e)).into_response();
        }
    };

    let status = upstream.status();
    let upstream_headers = upstream.headers().clone();

    state.audit.write(AuditEvent {
        account: Some(account.name.clone()),
        provider: Some("anthropic".into()),
        model,
        status: Some(status.as_u16()),
        stream: Some(stream),
        ..AuditEvent::new("messages_request")
    });

    let body_stream = upstream.bytes_stream();
    let mut builder = Response::builder().status(status);
    for k in ["content-type", "anthropic-request-id"] {
        if let Some(v) = upstream_headers.get(k) {
            builder = builder.header(k, v);
        }
    }
    builder.body(Body::from_stream(body_stream)).unwrap_or_else(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("resp build: {}", e),
        )
            .into_response()
    })
}
