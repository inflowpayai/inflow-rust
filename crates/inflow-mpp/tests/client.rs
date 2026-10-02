use http::{HeaderMap, Method};
use inflow_core::{
    Authentication, ClientOptions, Environment, Transport, TransportError, TransportRequest,
    TransportResponse,
};
use inflow_mpp::internal::MppClient;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct TransportStub {
    requests: Mutex<Vec<TransportRequest>>,
    responses: Mutex<VecDeque<Result<TransportResponse, TransportError>>>,
}
impl Transport for TransportStub {
    fn send(
        &self,
        request: TransportRequest,
    ) -> Pin<Box<dyn Future<Output = Result<TransportResponse, TransportError>> + Send + '_>> {
        self.requests.lock().unwrap().push(request);
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected request");
        Box::pin(async move { response })
    }
}
fn response(status: u16, body: Value) -> Result<TransportResponse, TransportError> {
    Ok(TransportResponse {
        status,
        headers: HeaderMap::new(),
        body: body.to_string().into_bytes(),
    })
}
fn client(
    responses: Vec<Result<TransportResponse, TransportError>>,
) -> (MppClient, Arc<TransportStub>) {
    let transport = Arc::new(TransportStub {
        responses: Mutex::new(responses.into()),
        ..Default::default()
    });
    let client = MppClient::new(ClientOptions {
        environment: Environment::Sandbox,
        authentication: Authentication::ApiKey("test-key".into()),
        transport: Some(transport.clone()),
        ..Default::default()
    })
    .unwrap();
    (client, transport)
}

#[tokio::test(start_paused = true)]
async fn endpoints_keep_validation_and_broadcast_separate() {
    let (client, transport) = client(vec![
        response(200, json!({"sellerId":"s"})),
        response(200, json!({"kinds":[]})),
        response(200, json!({"state":"pending"})),
        response(200, json!({"state":"ready"})),
        response(200, json!({"credential":"encoded"})),
        response(200, json!({"success":true})),
        response(503, json!({})),
        response(200, json!({"receipt":{"reference":"r"}})),
    ]);
    let token = CancellationToken::new();
    assert_eq!(client.config(&token).await.unwrap()["sellerId"], "s");
    assert_eq!(client.supported(&token).await.unwrap()["kinds"], json!([]));
    client
        .create(json!({"challenge":{"id":"c"},"options":{}}), &token)
        .await
        .unwrap();
    client.transaction("t /?", &token).await.unwrap();
    client
        .authorize("sub", json!({"id":"c"}), &token)
        .await
        .unwrap();
    client
        .validate(json!({"payload":{"x":1}}), &token)
        .await
        .unwrap();
    assert_eq!(transport.requests.lock().unwrap().len(), 6);
    client
        .broadcast(json!({"payload":{"x":1}}), "same-key", &token)
        .await
        .unwrap();
    let requests = transport.requests.lock().unwrap();
    let paths = [
        "/v1/mpp/config",
        "/v1/transactions/mpp-supported",
        "/v1/transactions/mpp",
        "/v1/transactions/t%20%2F%3F/mpp",
        "/v1/subscriptions/sub/authorize",
        "/v1/mpp/validate",
        "/v1/mpp/broadcast",
        "/v1/mpp/broadcast",
    ];
    for (i, (request, path)) in requests.iter().zip(paths).enumerate() {
        assert_eq!(request.url, format!("https://sandbox.inflowpay.ai{path}"));
        assert_eq!(request.headers["x-api-key"], "test-key");
        assert_eq!(
            request.method,
            if matches!(i, 0 | 1 | 3) {
                Method::GET
            } else {
                Method::POST
            }
        );
    }
    assert_eq!(
        serde_json::from_slice::<Value>(&requests[4].body).unwrap(),
        json!({"challenge":{"id":"c"}})
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&requests[5].body).unwrap(),
        json!({"credential":{"payload":{"x":1}}})
    );
    assert_eq!(requests[6].headers["idempotency-key"], "same-key");
    assert_eq!(requests[6].body, requests[7].body);
    assert_eq!(requests[6].headers, requests[7].headers);
}

#[tokio::test(start_paused = true)]
async fn creation_and_authorization_never_retry_and_client_remains_usable() {
    let (client, transport) = client(vec![
        response(503, json!({"detail":"not available"})),
        Err(TransportError::Network),
        response(200, json!({})),
    ]);
    let token = CancellationToken::new();
    assert_eq!(
        client
            .create(json!({}), &token)
            .await
            .unwrap_err()
            .http_status,
        503
    );
    assert_eq!(
        client
            .authorize("id", json!({}), &token)
            .await
            .unwrap_err()
            .code,
        "NETWORK_ERROR"
    );
    client.config(&token).await.unwrap();
    assert_eq!(transport.requests.lock().unwrap().len(), 3);
    for id in ["", ".", ".."] {
        assert!(client.transaction(id, &token).await.is_err());
        assert!(client.authorize(id, json!({}), &token).await.is_err());
    }
    for key in ["", " ", "\r\n"] {
        assert!(client.broadcast(json!({}), key, &token).await.is_err());
    }
    assert!(client.broadcast(json!({}), "a\nb", &token).await.is_err());
    token.cancel();
    assert_eq!(
        client.supported(&token).await.unwrap_err().code,
        "CANCELLED"
    );
    assert_eq!(transport.requests.lock().unwrap().len(), 3);
    assert!(
        MppClient::new(ClientOptions {
            authentication: Authentication::ApiKey("".into()),
            ..Default::default()
        })
        .is_err()
    );
}

#[tokio::test(start_paused = true)]
async fn reads_retry_three_times_and_preserve_errors() {
    let (client, transport) = client(
        (0..4)
            .map(|_| response(503, json!({"code":"BUSY","detail":"Try later"})))
            .collect(),
    );
    let error = client.config(&CancellationToken::new()).await.unwrap_err();
    assert_eq!(transport.requests.lock().unwrap().len(), 4);
    assert_eq!(error.code, "BUSY");
    assert_eq!(error.message, "Try later");
}
