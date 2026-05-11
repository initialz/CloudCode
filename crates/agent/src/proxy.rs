use crate::{auth, AppState};
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

const MAX_BODY: usize = 32 * 1024 * 1024;

pub async fn messages(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    req: Request<Body>,
) -> Response {
    // Hub-to-agent auth: Bearer <shared_secret>
    let Some(presented) = auth::extract_secret(&headers) else {
        return (StatusCode::UNAUTHORIZED, "missing bearer").into_response();
    };
    if !auth::verify_secret(&presented, &state.config.auth.shared_secret_hash) {
        return (StatusCode::UNAUTHORIZED, "invalid bearer").into_response();
    }

    let body_bytes = match axum::body::to_bytes(req.into_body(), MAX_BODY).await {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("body: {}", e)).into_response(),
    };

    let creds = state.credentials.snapshot();
    let url = format!(
        "{}/v1/messages",
        state.config.claude.upstream.trim_end_matches('/')
    );

    let beta_value = state.config.claude.anthropic_beta.join(",");
    let mut builder = state
        .http
        .post(&url)
        .header("authorization", format!("Bearer {}", creds.access_token))
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json");

    if !beta_value.is_empty() {
        // forward client's anthropic-beta plus our oauth-required flag
        let combined = match headers.get("anthropic-beta").and_then(|v| v.to_str().ok()) {
            Some(client_beta) if !client_beta.is_empty() => {
                format!("{},{}", client_beta, beta_value)
            }
            _ => beta_value.clone(),
        };
        builder = builder.header("anthropic-beta", combined);
    }

    let upstream = match builder.body(body_bytes.to_vec()).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "upstream request failed");
            return (StatusCode::BAD_GATEWAY, format!("upstream: {}", e)).into_response();
        }
    };

    let status = upstream.status();
    let upstream_headers = upstream.headers().clone();
    let body_stream = upstream.bytes_stream();

    let mut resp_builder = Response::builder().status(status);
    for k in ["content-type", "anthropic-request-id"] {
        if let Some(v) = upstream_headers.get(k) {
            resp_builder = resp_builder.header(k, v);
        }
    }
    resp_builder
        .body(Body::from_stream(body_stream))
        .unwrap_or_else(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("resp build: {}", e),
            )
                .into_response()
        })
}
