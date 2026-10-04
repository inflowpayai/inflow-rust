use http::{HeaderMap, HeaderValue, Method};
use inflow_core::internal::{ApprovalCleanup, HttpClient, poll};
use inflow_core::{
    AccessTokenProvider, Authentication, ClientOptions, Environment, Error, Transport,
    TransportError, TransportRequest, TransportResponse,
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio_util::sync::CancellationToken;

type Reply = Result<TransportResponse, TransportError>;

struct Script {
    requests: Mutex<Vec<TransportRequest>>,
    replies: Mutex<VecDeque<Reply>>,
    stall: bool,
}

impl Transport for Script {
    fn send(&self, request: TransportRequest) -> Pin<Box<dyn Future<Output = Reply> + Send + '_>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(request);
            if self.stall {
                std::future::pending::<()>().await;
            }
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected extra request")
        })
    }
}

fn reply(status: u16, body: &[u8]) -> Reply {
    Ok(TransportResponse {
        status,
        headers: HeaderMap::new(),
        body: body.to_vec(),
    })
}

fn setup(
    replies: Vec<Reply>,
    authentication: Authentication,
    stall: bool,
) -> (HttpClient, Arc<Script>) {
    let transport = Arc::new(Script {
        requests: Mutex::new(Vec::new()),
        replies: Mutex::new(replies.into()),
        stall,
    });
    let client = HttpClient::new(ClientOptions {
        environment: Environment::Sandbox,
        authentication,
        transport: Some(transport.clone()),
        ..Default::default()
    })
    .unwrap();
    (client, transport)
}

async fn get(client: &HttpClient, retries: u8) -> Result<Value, Error> {
    client
        .request(
            Method::GET,
            "/v1/example",
            None,
            HeaderMap::new(),
            retries,
            &CancellationToken::new(),
        )
        .await
}

#[tokio::test]
async fn request_headers_body_and_environment_are_isolated() {
    let (client, transport) = setup(
        vec![reply(200, br#"{"ok":true}"#), reply(204, b"")],
        Authentication::ApiKey("test-key".into()),
        false,
    );
    let headers = HeaderMap::from_iter([(
        "x-custom".parse().unwrap(),
        HeaderValue::from_static("value"),
    )]);
    assert_eq!(
        client
            .request(
                Method::POST,
                "/v1/example",
                Some(json!({"hello":"world"})),
                headers.clone(),
                0,
                &CancellationToken::new()
            )
            .await
            .unwrap(),
        json!({"ok":true})
    );
    assert_eq!(get(&client, 0).await.unwrap(), Value::Null);
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests[0].url, "https://sandbox.inflowpay.ai/v1/example");
    assert_eq!(requests[0].headers["x-api-key"], "test-key");
    assert!(requests[0].headers["x-api-key"].is_sensitive());
    assert!(!requests[0].headers.contains_key("authorization"));
    assert_eq!(requests[0].headers["content-type"], "application/json");
    assert_eq!(
        requests[0].headers["user-agent"],
        concat!("inflow-rust/", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(requests[0].headers["x-custom"], "value");
    assert_eq!(
        serde_json::from_slice::<Value>(&requests[0].body).unwrap(),
        json!({"hello":"world"})
    );
    assert!(!requests[1].headers.contains_key("content-type"));
    assert_eq!(headers.len(), 1);
}

struct Tokens {
    calls: AtomicUsize,
    fail: bool,
    invalid: bool,
}
impl AccessTokenProvider for Tokens {
    fn access_token(&self) -> Pin<Box<dyn Future<Output = Result<String, Error>> + Send + '_>> {
        Box::pin(async move {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(Error::new("PROVIDER", "token unavailable"));
            }
            Ok(if self.invalid {
                String::new()
            } else {
                format!("test-token-{n}")
            })
        })
    }
}

#[tokio::test(start_paused = true)]
async fn retry_refreshes_tokens_and_redacts_all_attempts() {
    let tokens = Arc::new(Tokens {
        calls: AtomicUsize::new(0),
        fail: false,
        invalid: false,
    });
    let headers = HeaderMap::from_iter([
        (
            "set-cookie".parse().unwrap(),
            HeaderValue::from_static("private"),
        ),
        (
            "x-request-id".parse().unwrap(),
            HeaderValue::from_static("request-1"),
        ),
        (
            "x-echo".parse().unwrap(),
            HeaderValue::from_static("test-token-0"),
        ),
    ]);
    let body = json!({"errors":[{"code":"DENIED","message":"test-token-0 and test-token-1"}],"detail":"test-token-0 and test-token-1", "nested":[{"api_key":"private","normal":true}]});
    let (client, transport) = setup(
        vec![
            reply(503, b""),
            Ok(TransportResponse {
                status: 403,
                headers,
                body: body.to_string().into_bytes(),
            }),
        ],
        Authentication::Bearer(tokens.clone()),
        false,
    );
    let error = get(&client, 3).await.unwrap_err();
    assert_eq!(tokens.calls.load(Ordering::SeqCst), 2);
    assert_eq!(error.code, "DENIED");
    assert_eq!(error.message, "[REDACTED] and [REDACTED]");
    assert_eq!(error.request_id.as_deref(), Some("request-1"));
    assert_eq!(error.http_status, 403);
    assert_eq!(error.endpoint, "/v1/example");
    assert!(!error.headers.contains_key("set-cookie"));
    assert_eq!(error.headers["x-echo"], "[REDACTED]");
    assert!(!format!("{error:?}").contains("test-token"));
    assert!(!format!("{error:?}").contains("private"));
    assert_eq!(error.to_string(), error.message);
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests[0].headers["authorization"], "Bearer test-token-0");
    assert!(requests[0].headers["authorization"].is_sensitive());
    assert_eq!(requests[1].headers["authorization"], "Bearer test-token-1");
    assert!(!requests[0].headers.contains_key("x-api-key"));
}

#[tokio::test]
async fn provider_failures_and_invalid_credentials_never_send_or_retry() {
    for invalid in [false, true] {
        let tokens = Arc::new(Tokens {
            calls: AtomicUsize::new(0),
            fail: !invalid,
            invalid,
        });
        let (client, transport) = setup(vec![], Authentication::Bearer(tokens.clone()), false);
        assert_eq!(
            get(&client, 3).await.unwrap_err().code,
            if invalid {
                "INVALID_CONFIGURATION"
            } else {
                "PROVIDER"
            }
        );
        assert_eq!(tokens.calls.load(Ordering::SeqCst), 1);
        assert!(transport.requests.lock().unwrap().is_empty());
    }
    for key in ["", "bad key", "bad\r\nkey", "é"] {
        assert!(
            HttpClient::new(ClientOptions {
                authentication: Authentication::ApiKey(key.into()),
                ..Default::default()
            })
            .is_err()
        );
    }
    assert!(
        HttpClient::new(ClientOptions {
            timeout: Duration::ZERO,
            ..Default::default()
        })
        .is_err()
    );
    assert!(HttpClient::new(ClientOptions::default()).is_ok());
}

#[tokio::test(start_paused = true)]
async fn retry_permission_is_explicit_and_capped() {
    for status in [429, 502, 503, 504] {
        let (client, transport) = setup(
            (0..4).map(|_| reply(status, b"")).collect(),
            Authentication::Anonymous,
            false,
        );
        assert_eq!(get(&client, 255).await.unwrap_err().http_status, status);
        assert_eq!(transport.requests.lock().unwrap().len(), 4);
        let (client, transport) = setup(vec![reply(status, b"")], Authentication::Anonymous, false);
        assert!(
            client
                .request(
                    Method::POST,
                    "/v1/create",
                    None,
                    HeaderMap::new(),
                    0,
                    &CancellationToken::new()
                )
                .await
                .is_err()
        );
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
    }
    for failure in [TransportError::Network, TransportError::Timeout] {
        let (client, _) = setup(
            vec![Err(failure), reply(200, b"text")],
            Authentication::Anonymous,
            false,
        );
        assert_eq!(get(&client, 1).await.unwrap(), json!("text"));
    }
    for status in [301, 401, 403, 404, 500] {
        let (client, transport) = setup(vec![reply(status, b"")], Authentication::Anonymous, false);
        assert_eq!(get(&client, 3).await.unwrap_err().code, "UNEXPECTED_ERROR");
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn invalid_paths_headers_and_oversized_bodies_fail_before_send() {
    let (client, transport) = setup(vec![], Authentication::Anonymous, false);
    for path in ["", "https://foreign/", "//foreign/", "/x#y", "/\\foreign"] {
        assert!(
            client
                .request(
                    Method::GET,
                    path,
                    None,
                    HeaderMap::new(),
                    0,
                    &CancellationToken::new()
                )
                .await
                .is_err()
        );
    }
    for name in [
        "Authorization",
        "x-api-key",
        "cookie",
        "host",
        "proxy-authorization",
    ] {
        let mut headers = HeaderMap::new();
        headers.insert(
            name.parse::<http::HeaderName>().unwrap(),
            HeaderValue::from_static("x"),
        );
        assert!(
            client
                .request(
                    Method::GET,
                    "/",
                    None,
                    headers,
                    0,
                    &CancellationToken::new()
                )
                .await
                .is_err()
        );
    }
    assert_eq!(
        client
            .request(
                Method::POST,
                "/",
                Some(json!("x".repeat(8 * 1024 * 1024))),
                HeaderMap::new(),
                0,
                &CancellationToken::new()
            )
            .await
            .unwrap_err()
            .code,
        "BODY_TOO_LARGE"
    );
    assert!(transport.requests.lock().unwrap().is_empty());
    for response in [
        Err(TransportError::BodyTooLarge),
        reply(200, &vec![0; 8 * 1024 * 1024 + 1]),
    ] {
        let (client, transport) = setup(vec![response], Authentication::Anonymous, false);
        assert_eq!(get(&client, 3).await.unwrap_err().code, "BODY_TOO_LARGE");
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn cancellation_interrupts_pending_io_and_backoff() {
    let (client, transport) = setup(vec![], Authentication::Anonymous, true);
    assert_eq!(get(&client, 0).await.unwrap_err().code, "TIMEOUT");
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
    for stall in [true, false] {
        let (client, transport) = setup(vec![reply(503, b"")], Authentication::Anonymous, stall);
        let token = CancellationToken::new();
        let future = client.request(Method::GET, "/", None, HeaderMap::new(), 3, &token);
        tokio::pin!(future);
        tokio::select! {
            result = &mut future => panic!("completed early: {result:?}"),
            _ = tokio::task::yield_now() => {}
        }
        token.cancel();
        assert_eq!(future.await.unwrap_err().code, "CANCELLED");
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
    }
    let token = CancellationToken::new();
    token.cancel();
    let (client, transport) = setup(vec![], Authentication::Anonymous, false);
    assert_eq!(
        client
            .request(Method::GET, "/", None, HeaderMap::new(), 0, &token)
            .await
            .unwrap_err()
            .code,
        "CANCELLED"
    );
    assert!(transport.requests.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn polling_respects_deadlines_terminal_values_and_failures() {
    for (case, expected) in [
        ("complete", None),
        ("stall", Some("TIMEOUT")),
        ("zero_timeout", Some("INVALID_CONFIGURATION")),
        ("zero_interval", Some("INVALID_CONFIGURATION")),
        ("read_error", Some("READ")),
        ("cancel_during_read", Some("CANCELLED")),
        ("cancel_before_read", Some("CANCELLED")),
    ] {
        let token = CancellationToken::new();
        if case == "cancel_before_read" {
            token.cancel();
        }
        let timeout = if case == "zero_timeout" {
            Duration::ZERO
        } else {
            Duration::from_secs(1)
        };
        let mut n = 0;
        let result = poll(timeout, &token, || {
            n += 1;
            let n = n;
            let token = token.clone();
            async move {
                if case == "stall" {
                    std::future::pending::<()>().await;
                }
                if case == "read_error" {
                    return Err(Error::new("READ", "failed"));
                }
                if case == "cancel_during_read" {
                    token.cancel();
                }
                let delay = if case == "zero_interval" {
                    Duration::ZERO
                } else {
                    Duration::from_millis(10)
                };
                Ok((n, n == 2, delay))
            }
        })
        .await;
        if let Some(code) = expected {
            assert_eq!(result.unwrap_err().code, code, "{case}");
        } else {
            assert_eq!(result.unwrap(), 2);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn approval_cleanup_is_shared_bounded_and_disarmed_after_completion() {
    let (client, transport) = setup(vec![reply(204, b"")], Authentication::Anonymous, false);
    let guard = ApprovalCleanup::new(client, "id/with space").unwrap();
    let token = guard.cancellation();
    let (a, b) = tokio::join!(guard.cancel(), guard.cancel());
    a.unwrap();
    b.unwrap();
    assert!(token.is_cancelled());
    drop(guard);
    tokio::task::yield_now().await;
    {
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].url.ends_with("/id%2Fwith%20space/cancel"));
    }
    let (client, transport) = setup(vec![], Authentication::Anonymous, false);
    let mut guard = ApprovalCleanup::new(client, "id").unwrap();
    guard.disarm();
    guard.cancel().await.unwrap();
    drop(guard);
    tokio::task::yield_now().await;
    assert!(transport.requests.lock().unwrap().is_empty());
    let (client, transport) = setup(vec![reply(204, b"")], Authentication::Anonymous, false);
    drop(ApprovalCleanup::new(client, "id").unwrap());
    tokio::task::yield_now().await;
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
    let (client, _) = setup(vec![], Authentication::Anonymous, true);
    let guard = ApprovalCleanup::new(client, "id").unwrap();
    assert_eq!(guard.cancel().await.unwrap_err().code, "TIMEOUT");
    let (client, _) = setup(vec![reply(500, b"")], Authentication::Anonymous, false);
    let guard = ApprovalCleanup::new(client, "id").unwrap();
    assert_eq!(guard.cancel().await.unwrap_err().http_status, 500);
    let (client, _) = setup(vec![], Authentication::Anonymous, false);
    assert!(ApprovalCleanup::new(client, "").is_err());
}

#[test]
fn approval_cleanup_requires_the_callers_runtime() {
    let (client, _) = setup(vec![], Authentication::Anonymous, false);
    assert!(ApprovalCleanup::new(client, "id").is_err());
}

struct CancelHere(CancellationToken);

impl AccessTokenProvider for CancelHere {
    fn access_token(&self) -> Pin<Box<dyn Future<Output = Result<String, Error>> + Send + '_>> {
        Box::pin(async move {
            self.0.cancel();
            Ok("test-token".into())
        })
    }
}

impl Transport for CancelHere {
    fn send(&self, _: TransportRequest) -> Pin<Box<dyn Future<Output = Reply> + Send + '_>> {
        Box::pin(async move {
            self.0.cancel();
            reply(200, b"{}")
        })
    }
}

#[tokio::test]
async fn cancellation_from_immediate_provider_or_transport_is_not_lost() {
    let token = CancellationToken::new();
    let (client, transport) = setup(
        vec![],
        Authentication::Bearer(Arc::new(CancelHere(token.clone()))),
        false,
    );
    assert_eq!(
        client
            .request(Method::GET, "/", None, HeaderMap::new(), 0, &token)
            .await
            .unwrap_err()
            .code,
        "CANCELLED"
    );
    assert!(transport.requests.lock().unwrap().is_empty());
    let token = CancellationToken::new();
    let client = HttpClient::new(ClientOptions {
        transport: Some(Arc::new(CancelHere(token.clone()))),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        client
            .request(Method::GET, "/", None, HeaderMap::new(), 0, &token)
            .await
            .unwrap_err()
            .code,
        "CANCELLED"
    );
}

#[tokio::test]
async fn errors_preserve_structured_messages_and_empty_body_fallbacks() {
    for (body, code, message) in [
        (
            json!({"code":"TOP","message":"top message"}),
            "TOP",
            "top message",
        ),
        (
            json!({"errors":[{"code":"FIRST","message":"first message"}]}),
            "FIRST",
            "first message",
        ),
        (
            json!({"errors":[],"code":"","message":""}),
            "UNEXPECTED_ERROR",
            "request failed",
        ),
        (json!("plain failure"), "UNEXPECTED_ERROR", "request failed"),
        (
            json!({"errors":[null],"detail":"explanation"}),
            "UNEXPECTED_ERROR",
            "explanation",
        ),
    ] {
        let (client, _) = setup(
            vec![reply(400, body.to_string().as_bytes())],
            Authentication::Anonymous,
            false,
        );
        let error = get(&client, 0).await.unwrap_err();
        assert_eq!(error.code, code);
        assert_eq!(error.message, message);
        assert_eq!(*error.body, body);
    }
    for id in [".", ".."] {
        let (client, _) = setup(vec![], Authentication::Anonymous, false);
        assert!(ApprovalCleanup::new(client, id).is_err());
    }
}

#[tokio::test(start_paused = true)]
async fn concurrent_cancellation_shares_the_in_flight_timeout() {
    let (client, transport) = setup(vec![], Authentication::Anonymous, true);
    let guard = ApprovalCleanup::new(client, "approval").unwrap();
    let (first, second) = tokio::join!(guard.cancel(), guard.cancel());
    assert_eq!(first.unwrap_err().code, "TIMEOUT");
    assert_eq!(second.unwrap_err().code, "TIMEOUT");
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn seller_account_errors_preserve_the_runtime_contract_message() {
    let message = "The supplied credentials belong to a Developer account. This endpoint requires a Seller account.";
    let (client, _) = setup(vec![reply(403,json!({"errors":[{"code":"SELLER_ACCOUNT_REQUIRED","message":message}],"id":"44444444-4444-4444-8444-444444444444"}).to_string().as_bytes())],Authentication::ApiKey("test-only-developer-key".into()),false);
    let error = client
        .request(
            Method::GET,
            "/v1/mpp/config",
            None,
            HeaderMap::new(),
            0,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.http_status, 403);
    assert_eq!(error.code, "SELLER_ACCOUNT_REQUIRED");
    assert_eq!(error.message, message);
}
