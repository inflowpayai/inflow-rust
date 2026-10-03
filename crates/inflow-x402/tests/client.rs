use http::{HeaderMap, Method};
use inflow_core::{
    Authentication, ClientOptions, Environment, Transport, TransportError, TransportRequest,
    TransportResponse,
};
use inflow_x402::{
    facilitator_request, identifier_declaration, identifier_entry, internal::X402Client,
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Stub {
    requests: Mutex<Vec<TransportRequest>>,
    responses: Mutex<VecDeque<Result<TransportResponse, TransportError>>>,
}
impl Transport for Stub {
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
fn setup(
    responses: Vec<Result<TransportResponse, TransportError>>,
    auth: Authentication,
) -> (X402Client, Arc<Stub>) {
    let transport = Arc::new(Stub {
        responses: Mutex::new(responses.into()),
        ..Default::default()
    });
    let client = X402Client::new(ClientOptions {
        environment: Environment::Sandbox,
        authentication: auth,
        transport: Some(transport.clone()),
        ..Default::default()
    })
    .unwrap();
    (client, transport)
}
fn payload() -> Value {
    json!({"x402Version":2,"accepted":{"scheme":"balance","network":"inflow:1"},"payload":{"transactionId":"00000000-0000-4000-8000-000000000001"},"extensions":{"future":{"keep":true}}})
}
fn request() -> inflow_x402::VerifyRequest {
    facilitator_request(&payload(), &json!({"scheme":"balance","amount":"100"})).unwrap()
}

#[tokio::test(start_paused = true)]
async fn endpoints_roles_and_retry_boundaries() {
    let token = CancellationToken::new();
    let (client, stub) = setup(
        vec![
            response(503, json!({})),
            response(200, json!({"sellerId":"s"})),
            response(200, json!({"kinds":[],"extensions":["future"]})),
            response(200, json!({"kinds":[]})),
            response(200, json!({"balances":[]})),
            response(200, json!({"approvalId":"a"})),
            response(200, json!({"status":"PENDING"})),
            response(204, Value::Null),
        ],
        Authentication::ApiKey("seller-key".into()),
    );
    assert_eq!(client.config(&token).await.unwrap()["sellerId"], "s");
    assert_eq!(
        client.supported(&token).await.unwrap()["extensions"],
        json!(["future"])
    );
    client.buyer_supported(&token).await.unwrap();
    client.balances(&token).await.unwrap();
    client
        .create(json!({"accept":{"amount":"100"}}), &token)
        .await
        .unwrap();
    client.poll("id /?é", &token).await.unwrap();
    client
        .approval_cleanup("approval")
        .unwrap()
        .cancel()
        .await
        .unwrap();
    let requests = stub.requests.lock().unwrap();
    let paths = [
        "/v1/x402/config",
        "/v1/x402/config",
        "/v1/x402/supported",
        "/v1/transactions/x402-supported",
        "/v1/balances",
        "/v1/transactions/x402",
        "/v1/transactions/id%20%2F%3F%C3%A9/x402",
        "/v1/approvals/approval/cancel",
    ];
    assert_eq!(requests.len(), paths.len());
    for (index, (r, path)) in requests.iter().zip(paths).enumerate() {
        assert_eq!(r.url, format!("https://sandbox.inflowpay.ai{path}"));
        assert_eq!(r.headers["x-api-key"], "seller-key");
        assert_eq!(
            r.method,
            if matches!(index, 5 | 7) {
                Method::POST
            } else {
                Method::GET
            }
        );
    }
    assert_eq!(
        serde_json::from_slice::<Value>(&requests[5].body).unwrap(),
        json!({"accept":{"amount":"100"}})
    );
}

#[tokio::test(start_paused = true)]
async fn creation_poll_verification_and_settlement_do_not_retry_transport_failures() {
    for status in [401, 403, 409, 429, 500, 502, 503, 504] {
        let body = json!({"errorReason":"not_pending","detail":"Failure","extra":7});
        let (client, stub) = setup(
            (0..4).map(|_| response(status, body.clone())).collect(),
            Authentication::Anonymous,
        );
        let token = CancellationToken::new();
        for error in [
            client.create(json!({}), &token).await.unwrap_err(),
            client.poll("id", &token).await.unwrap_err(),
            client.verify(&request(), &token).await.unwrap_err(),
            client.settle(&request(), &token).await.unwrap_err(),
        ] {
            assert_eq!(error.http_status, status);
            assert_eq!(*error.body, body);
        }
        let requests = stub.requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        assert!(
            requests
                .iter()
                .all(|r| !r.headers.contains_key("authorization")
                    && !r.headers.contains_key("x-api-key"))
        );
    }
    let (client, stub) = setup(
        vec![Err(TransportError::Network), Err(TransportError::Timeout)],
        Authentication::Anonymous,
    );
    assert_eq!(
        client
            .verify(&request(), &CancellationToken::new())
            .await
            .unwrap_err()
            .code,
        "NETWORK_ERROR"
    );
    assert_eq!(
        client
            .settle(&request(), &CancellationToken::new())
            .await
            .unwrap_err()
            .code,
        "TIMEOUT"
    );
    assert_eq!(stub.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn responses_preserve_false_outcomes_and_all_extensions() {
    let verify = json!({"isValid":false,"invalidReason":"permit2_allowance_required","invalidMessage":"Approve allowance","extensions":{"sponsor":{"nonce":"1"}}});
    let settle = json!({"success":false,"errorReason":"declined","errorMessage":"Declined","network":"inflow:1","transaction":"","amount":"100","extensions":{"future":1},"transactionFee":"0.01"});
    let (client, stub) = setup(
        vec![
            response(412, verify.clone()),
            response(200, settle.clone()),
            response(
                200,
                json!({"isValid":true,"payer":"payer","extensions":{"future":1}}),
            ),
            response(
                200,
                json!({"success":true,"transaction":"tx","amount":"100"}),
            ),
        ],
        Authentication::Anonymous,
    );
    let before = request();
    let bytes = before.as_str().to_owned();
    let token = CancellationToken::new();
    assert_eq!(client.verify(&before, &token).await.unwrap().0, verify);
    assert_eq!(client.settle(&before, &token).await.unwrap().0, settle);
    assert_eq!(
        client.verify(&before, &token).await.unwrap().0["extensions"],
        json!({"future":1})
    );
    assert_eq!(
        client.settle(&before, &token).await.unwrap().0["amount"],
        "100"
    );
    assert_eq!(before.as_str(), bytes);
    let requests = stub.requests.lock().unwrap();
    assert!(requests.windows(2).all(|r| r[0].body == r[1].body));
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
        sent["paymentPayload"]["extensions"]["future"],
        json!({"keep":true})
    );
    assert_eq!(
        sent["paymentPayload"]["extensions"]["payment-identifier"]["info"]["required"],
        false
    );
}

#[tokio::test]
async fn malformed_responses_are_not_success_or_allowance_results() {
    for body in [
        Value::Null,
        json!({}),
        json!({"isValid":"false","invalidReason":"permit2_allowance_required"}),
        json!({"isValid":true,"invalidReason":"permit2_allowance_required"}),
        json!({"isValid":false,"invalidReason":"other"}),
    ] {
        let (client, _) = setup(vec![response(412, body.clone())], Authentication::Anonymous);
        let error = client
            .verify(&request(), &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.http_status, 412);
        assert_eq!(*error.body, body);
    }
    for body in [
        Value::Null,
        json!({}),
        json!({"isValid":"true","success":1}),
    ] {
        let (client, _) = setup(
            vec![response(200, body.clone()), response(200, body)],
            Authentication::Anonymous,
        );
        assert_eq!(
            client
                .verify(&request(), &CancellationToken::new())
                .await
                .unwrap_err()
                .code,
            "INVALID_X402_DATA"
        );
        assert_eq!(
            client
                .settle(&request(), &CancellationToken::new())
                .await
                .unwrap_err()
                .code,
            "INVALID_X402_DATA"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn pending_settlement_is_bounded_and_uses_identical_requests() {
    for (header, seconds) in [
        (None, 5),
        (Some("0"), 0),
        (Some("2"), 2),
        (Some("100"), 5),
        (Some("bad"), 5),
        (Some(""), 5),
        (Some("+1"), 5),
        (Some("999999999999999999999999"), 5),
    ] {
        let mut pending = response(409, json!({"errorReason":"idempotency_pending"})).unwrap();
        if let Some(header) = header {
            pending
                .headers
                .insert("retry-after", header.parse().unwrap());
        }
        let (client, stub) = setup(
            vec![Ok(pending), response(200, json!({"success":true}))],
            Authentication::Anonymous,
        );
        let start = tokio::time::Instant::now();
        client
            .settle(&request(), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(start.elapsed(), Duration::from_secs(seconds));
        let requests = stub.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].body, requests[1].body);
    }
    let (client, stub) = setup(
        (0..5)
            .map(|_| response(409, json!({"errorReason":"idempotency_pending"})))
            .collect(),
        Authentication::Anonymous,
    );
    assert_eq!(
        client
            .settle(&request(), &CancellationToken::new())
            .await
            .unwrap_err()
            .http_status,
        409
    );
    assert_eq!(stub.requests.lock().unwrap().len(), 5);
    let (client, stub) = setup(
        vec![response(409, json!({"errorReason":"idempotency_pending"}))],
        Authentication::Anonymous,
    );
    let token = CancellationToken::new();
    let child = token.clone();
    let task = tokio::spawn(async move { client.settle(&request(), &child).await });
    tokio::task::yield_now().await;
    token.cancel();
    assert_eq!(task.await.unwrap().unwrap_err().code, "CANCELLED");
    assert_eq!(stub.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn identifiers_are_deterministic_and_existing_values_are_preserved() {
    let mut inputs = Vec::new();
    for data in [
        json!({"transactionId":"id"}),
        json!({"transaction":"encoded"}),
        json!({"signature":"sig"}),
        json!({"other":"opaque"}),
    ] {
        let mut p = payload();
        p["payload"] = data;
        inputs.push(p);
    }
    let mut p = payload();
    p["extensions"]["payment-identifier"] =
        identifier_entry(&identifier_declaration(), "pay_0123456789abcdef").unwrap();
    inputs.push(p);
    let mut p = payload();
    p.as_object_mut().unwrap().remove("extensions");
    inputs.push(p);
    let (client, stub) = setup(
        (0..inputs.len() * 2)
            .map(|_| response(200, json!({"isValid":true})))
            .collect(),
        Authentication::Anonymous,
    );
    for p in &inputs {
        let request = facilitator_request(p, &json!({})).unwrap();
        let before = request.as_str().to_owned();
        client
            .verify(&request, &CancellationToken::new())
            .await
            .unwrap();
        client
            .verify(&request, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(request.as_str(), before);
    }
    let requests = stub.requests.lock().unwrap();
    for pair in requests.chunks(2) {
        assert_eq!(pair[0].body, pair[1].body);
    }
    let sent: Vec<Value> = requests
        .iter()
        .step_by(2)
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(sent[4]["paymentPayload"], inputs[4]);
    // SHA-256 reference values produced with Node crypto using the same wire material.
    for (index, expected) in [
        "pay_82a48fa04b29ad5c62577a4d7fc1fd45",
        "pay_f842340fdabb584f29fbfd5367e7c7c9",
        "pay_ab9c662e88dbe8b3065009099ed59cac",
        "pay_ff82b76e37584c1553061f29ef9429ba",
    ]
    .iter()
    .enumerate()
    {
        assert_eq!(
            sent[index]["paymentPayload"]["extensions"]["payment-identifier"]["info"]["id"],
            *expected
        );
    }
    let ids: std::collections::HashSet<_> = sent[..4]
        .iter()
        .map(|v| {
            v["paymentPayload"]["extensions"]["payment-identifier"]["info"]["id"]
                .as_str()
                .unwrap()
        })
        .collect();
    assert_eq!(ids.len(), 4);
}

#[tokio::test]
async fn invalid_requests_and_pre_cancelled_calls_do_not_send() {
    let (client, stub) = setup(vec![], Authentication::Anonymous);
    let token = CancellationToken::new();
    let raw =
        serde_json::value::RawValue::from_string("{\"x402Version\":2,\"extra\":1e999}".into())
            .unwrap();
    let overflowing = inflow_x402::VerifyRequest::from(raw);
    assert_eq!(
        client.verify(&overflowing, &token).await.unwrap_err().code,
        "INVALID_X402_DATA"
    );
    for id in ["", ".", ".."] {
        assert!(client.poll(id, &token).await.is_err());
    }
    assert!(client.approval_cleanup("").is_err());
    for body in [
        json!({}),
        json!({"x402Version":1}),
        json!({"x402Version":2,"paymentPayload":{"x402Version":2,"payload":{},"accepted":{}},"paymentRequirements":null}),
        json!({"x402Version":2,"paymentPayload":{"x402Version":2,"payload":{},"accepted":{},"extensions":null},"paymentRequirements":{}}),
    ] {
        let request: inflow_x402::VerifyRequest = serde_json::from_value(body).unwrap();
        assert!(client.verify(&request, &token).await.is_err());
    }
    token.cancel();
    assert_eq!(
        client.verify(&request(), &token).await.unwrap_err().code,
        "CANCELLED"
    );
    assert_eq!(
        client.settle(&request(), &token).await.unwrap_err().code,
        "CANCELLED"
    );
    assert!(stub.requests.lock().unwrap().is_empty());
    assert!(
        X402Client::new(ClientOptions {
            authentication: Authentication::ApiKey("".into()),
            ..Default::default()
        })
        .is_err()
    );
}
