use crate::Result;
use axum::{
    Router,
    body::{Body, to_bytes},
    response::{IntoResponse, Response},
    routing::get,
};
use http::{Request, StatusCode};
use inflow_tap_seller::{Request as TapRequest, Verifier};
use std::sync::Arc;

pub fn router(verifier: Verifier, public_origin: &str) -> Result<Router> {
    let origin = url::Url::parse(public_origin)?;
    if !matches!(origin.scheme(), "http" | "https")
        || origin.host_str().is_none()
        || !origin.username().is_empty()
        || origin.password().is_some()
        || origin.path() != "/"
        || origin.query().is_some()
        || origin.fragment().is_some()
    {
        return Err("PUBLIC_ORIGIN must be an HTTP or HTTPS origin without credentials, path, query or fragment.".into());
    }
    let origin = origin.origin().ascii_serialization();
    let verifier = Arc::new(verifier);
    let handler = move |request: Request<Body>| {
        let verifier = verifier.clone();
        let origin = origin.clone();
        async move { handle(&verifier, &origin, request).await }
    };
    Ok(Router::new().route("/api/catalog", get(handler.clone()).post(handler)))
}

async fn handle(verifier: &Verifier, origin: &str, request: Request<Body>) -> Response {
    let (parts, body) = request.into_parts();
    // Use the configured public origin and original encoded URI, never forwarding headers.
    let url = format!(
        "{origin}{}",
        parts.uri.path_and_query().map_or("/", |v| v.as_str())
    );
    let supplied = parts.headers.contains_key("content-length")
        || parts.headers.contains_key("transfer-encoding");
    let body = match to_bytes(body, 1024 * 1024).await {
        Ok(body) => body,
        Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    };
    let request = TapRequest {
        method: parts.method.to_string(),
        url,
        headers: parts.headers,
        body: if supplied || !body.is_empty() {
            Some(body.to_vec())
        } else {
            None
        },
    };
    match verifier
        .with_verified(&request, |_| async {
            // TAP has verified the agent, not a buyer account or payment. Apply those checks separately.
            axum::Json(serde_json::json!({"items":["example catalog entry"]})).into_response()
        })
        .await
    {
        Ok(response) => response,
        Err(_) => StatusCode::UNAUTHORIZED.into_response(),
    }
}
