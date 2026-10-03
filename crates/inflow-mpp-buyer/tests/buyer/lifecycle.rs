use inflow_core::{Transport, TransportError, TransportRequest, TransportResponse};
use inflow_mpp::{Base64UrlJson, encode};
use inflow_mpp_buyer::*;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::Notify, time::Instant};

enum Reply {
    Json(u16, Value),
    Network,
    Hang(Arc<Notify>),
}
struct Script {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<(TransportRequest, Instant)>>,
}
impl Transport for Script {
    fn send(
        &self,
        request: TransportRequest,
    ) -> Pin<Box<dyn Future<Output = Result<TransportResponse, TransportError>> + Send + '_>> {
        self.requests
            .lock()
            .unwrap()
            .push((request, Instant::now()));
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected request");
        Box::pin(async move {
            match reply {
                Reply::Json(status, value) => Ok(TransportResponse {
                    status,
                    headers: Default::default(),
                    body: value.to_string().into_bytes(),
                }),
                Reply::Network => Err(TransportError::Network),
                Reply::Hang(started) => {
                    started.notify_one();
                    std::future::pending().await
                }
            }
        })
    }
}
fn client(replies: Vec<Reply>) -> (Buyer, Arc<Script>) {
    let script = Arc::new(Script {
        replies: Mutex::new(replies.into()),
        requests: Mutex::default(),
    });
    let buyer = Buyer::new(ClientOptions {
        authentication: Authentication::ApiKey("synthetic-key".into()),
        transport: Some(script.clone()),
        ..Default::default()
    })
    .unwrap();
    (buyer, script)
}
fn reply(value: Value) -> Reply {
    Reply::Json(200, value)
}
fn challenge() -> PaymentChallenge {
    PaymentChallenge::new(
        "id",
        "shop",
        "inflow",
        "charge",
        Base64UrlJson::from_raw(encode(&json!({"amount":"1","currency":"USDC"})).unwrap()),
    )
}
fn ready() -> Value {
    json!({"state":"ready", "transactionId":"tx", "credential":encode(&json!({"challenge":challenge(),"payload":{"proof":"synthetic"}})).unwrap()})
}
fn pending() -> Value {
    json!({"state":"pending","transactionId":"tx","approvalId":"approval"})
}
fn fast() -> WaitOptions {
    WaitOptions {
        poll_interval: Duration::ZERO,
        timeout: Duration::from_secs(10),
    }
}
async fn start(buyer: &Buyer) -> Payment {
    buyer
        .prepare(
            &challenge(),
            PaymentOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap()
}
fn error<T>(result: Result<T, Error>) -> Error {
    match result {
        Err(e) => e,
        Ok(_) => panic!("expected error"),
    }
}

#[tokio::test(start_paused = true)]
async fn cancellation_is_coalesced_and_completion_disarms_cleanup() {
    let (buyer, script) = client(vec![
        reply(pending()),
        reply(json!({})),
        reply(ready()),
        reply(ready()),
    ]);
    let payment = start(&buyer).await;
    assert_eq!(payment.approval_id(), Some("approval"));
    assert_eq!(payment.transaction_id(), Some("tx"));
    let (a, b) = tokio::join!(payment.cancel(), payment.cancel());
    a.unwrap();
    b.unwrap();
    payment.cancel().await.unwrap();
    assert_eq!(
        error(payment.wait(fast()).await).code,
        "MPP_PAYMENT_CANCELLED"
    );
    start(&buyer)
        .await
        .wait(WaitOptions::default())
        .await
        .unwrap();
    let payment = start(&buyer).await;
    assert_eq!(payment.approval_id(), None);
    payment.cancel().await.unwrap();
    assert_eq!(
        error(payment.wait(fast()).await).code,
        "MPP_PAYMENT_CANCELLED"
    );
    tokio::task::yield_now().await;
    assert_eq!(script.requests.lock().unwrap().len(), 4);
}

#[tokio::test(start_paused = true)]
async fn drop_pending_handle_or_wait_cleans_known_approval_once() {
    for waiting in [false, true] {
        let (buyer, script) = client(vec![reply(pending()), reply(json!({}))]);
        let payment = start(&buyer).await;
        if waiting {
            let mut wait = Box::pin(payment.wait(WaitOptions::default()));
            tokio::select! { biased; _ = &mut wait => panic!("must be waiting"), _=tokio::task::yield_now()=>{} }
            drop(wait);
        } else {
            drop(payment);
        }
        tokio::task::yield_now().await;
        assert_eq!(script.requests.lock().unwrap().len(), 2);
        assert!(
            script.requests.lock().unwrap()[1]
                .0
                .url
                .ends_with("/approvals/approval/cancel")
        );
    }
}

#[tokio::test(start_paused = true)]
async fn cancellation_is_per_payment_and_does_not_cancel_other_operations() {
    let (buyer, script) = client(vec![
        reply(pending()),
        reply(ready()),
        reply(json!({})),
        reply(ready()),
    ]);
    let pending = start(&buyer).await;
    let ready = start(&buyer).await;
    pending.cancellation().cancel();
    let (cancelled, success) = tokio::join!(pending.wait(fast()), ready.wait(fast()));
    assert_eq!(error(cancelled).code, "MPP_PAYMENT_CANCELLED");
    success.unwrap();
    start(&buyer).await.wait(fast()).await.unwrap();
    assert_eq!(script.requests.lock().unwrap().len(), 4);
}

#[tokio::test(start_paused = true)]
async fn explicit_cleanup_reports_errors_automatic_cleanup_preserves_original() {
    let (buyer, script) = client(vec![
        reply(pending()),
        Reply::Json(403, json!({"code":"DENIED"})),
    ]);
    let payment = start(&buyer).await;
    assert_eq!(payment.cancel().await.unwrap_err().code, "DENIED");
    assert_eq!(
        error(payment.wait(fast()).await).code,
        "MPP_PAYMENT_CANCELLED"
    );
    assert_eq!(script.requests.lock().unwrap().len(), 2);
    let (buyer, script) = client(vec![reply(pending()), Reply::Network, Reply::Network]);
    assert_eq!(
        error(start(&buyer).await.wait(fast()).await).code,
        "NETWORK_ERROR"
    );
    assert_eq!(script.requests.lock().unwrap().len(), 3);
    let hang = Arc::new(Notify::new());
    let (buyer, script) = client(vec![reply(pending()), Reply::Hang(hang)]);
    let payment = start(&buyer).await;
    let before = Instant::now();
    assert_eq!(payment.cancel().await.unwrap_err().code, "TIMEOUT");
    assert_eq!(before.elapsed(), Duration::from_secs(5));
    drop(payment);
    tokio::task::yield_now().await;
    assert_eq!(script.requests.lock().unwrap().len(), 2);
    let (buyer, script) = client(vec![reply(json!({}))]);
    buyer.cancel_approval("known /id").await.unwrap();
    assert!(
        script.requests.lock().unwrap()[0]
            .0
            .url
            .ends_with("known%20%2Fid/cancel")
    );
    assert!(buyer.cancel_approval("").await.is_err());
}

#[tokio::test(start_paused = true)]
async fn polling_advice_deadlines_and_invalid_responses() {
    for (advice, delay) in [(None, 3), (Some(json!(2)), 2)] {
        let mut value = pending();
        if let Some(advice) = advice {
            value["retryAfterSeconds"] = advice;
        }
        let (buyer, script) = client(vec![reply(value), reply(ready())]);
        start(&buyer)
            .await
            .wait(WaitOptions {
                poll_interval: Duration::from_secs(3),
                ..fast()
            })
            .await
            .unwrap();
        let requests = script.requests.lock().unwrap();
        assert_eq!(requests[1].1 - requests[0].1, Duration::from_secs(delay));
    }
    for timeout in [Duration::ZERO, Duration::MAX] {
        let (buyer, script) = client(vec![reply(pending()), reply(json!({}))]);
        assert!(
            start(&buyer)
                .await
                .wait(WaitOptions { timeout, ..fast() })
                .await
                .is_err()
        );
        assert_eq!(script.requests.lock().unwrap().len(), 2);
    }
    for bad in [json!(-1), json!("x"), json!(1e100)] {
        let mut value = pending();
        value["retryAfterSeconds"] = bad;
        let (buyer, _) = client(vec![reply(value), reply(json!({}))]);
        assert_eq!(
            error(start(&buyer).await.wait(fast()).await).code,
            "MPP_MALFORMED_CREDENTIAL"
        );
    }
    for response in [
        json!({}),
        json!({"state":"ready"}),
        json!({"state":"ready","credential":"invalid"}),
        json!({"state":"pending"}),
        json!({"state":"expired"}),
        json!({"state":"pending","transactionId":""}),
        json!({"state":"pending","transactionId":42}),
        json!({"state":"failed"}),
        json!({"state":"failed","problem":{"title":"Rejected"}}),
    ] {
        let (buyer, _) = client(vec![reply(response)]);
        assert!(start(&buyer).await.wait(fast()).await.is_err());
    }
    let (buyer, _) = client(vec![reply(json!({"state":"pending","approvalId":false}))]);
    assert!(
        buyer
            .prepare(
                &challenge(),
                PaymentOptions::default(),
                &CancellationToken::new()
            )
            .await
            .is_err()
    );
}

#[tokio::test(start_paused = true)]
async fn pending_budget_interrupts_a_poll_and_cancels_the_approval() {
    let (buyer, script) = client(vec![
        reply(pending()),
        Reply::Hang(Arc::new(Notify::new())),
        reply(json!({})),
    ]);
    let started = Instant::now();
    let failure = error(start(&buyer).await.wait(fast()).await);
    assert_eq!(failure.code, "MPP_PAYMENT_TIMEOUT");
    assert_eq!(*failure.body, json!({"transactionId":"tx"}));
    assert_eq!(Instant::now() - started, Duration::from_secs(10));
    let requests = script.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[2].0.url.ends_with("/approvals/approval/cancel"));
}

#[tokio::test(start_paused = true)]
async fn validate_inputs_and_never_retry_create_or_authorize() {
    let (buyer, script) = client(vec![]);
    for expiry in ["2000-01-01T00:00:00Z", "not-a-date"] {
        let mut challenge = challenge();
        challenge.expires = Some(expiry.into());
        assert_eq!(
            error(
                buyer
                    .prepare(
                        &challenge,
                        PaymentOptions::default(),
                        &CancellationToken::new()
                    )
                    .await
            )
            .code,
            "MPP_PAYMENT_EXPIRED"
        );
    }
    let guid = "11111111-1111-1111-1111-111111111111";
    for id in [
        "bad",
        "g1111111-1111-1111-1111-111111111111",
        "11111111x1111-1111-1111-111111111111",
    ] {
        assert!(
            buyer
                .prepare(
                    &challenge(),
                    PaymentOptions {
                        instrument_id: Some(id.into()),
                        ..Default::default()
                    },
                    &CancellationToken::new()
                )
                .await
                .is_err()
        );
    }
    assert!(
        buyer
            .prepare(
                &challenge(),
                PaymentOptions {
                    subscription_id: Some(guid.into()),
                    ..Default::default()
                },
                &CancellationToken::new()
            )
            .await
            .is_err()
    );
    let mut sub = challenge();
    sub.intent = "subscription".into();
    sub.request=Base64UrlJson::from_raw(encode(&json!({"amount":"1","currency":"USDC","periodUnit":"month","periodCount":1,"subscriptionExpires":"2099-01-01T00:00:00Z"})).unwrap());
    assert!(
        buyer
            .prepare(
                &sub,
                PaymentOptions {
                    instrument_id: Some(guid.into()),
                    ..Default::default()
                },
                &CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert!(script.requests.lock().unwrap().is_empty());
    let (buyer, script) = client(vec![
        Reply::Network,
        Reply::Json(503, json!({})),
        reply(ready()),
    ]);
    assert_eq!(
        error(
            buyer
                .prepare(
                    &challenge(),
                    PaymentOptions::default(),
                    &CancellationToken::new()
                )
                .await
        )
        .code,
        "NETWORK_ERROR"
    );
    assert_eq!(
        error(
            buyer
                .prepare(
                    &sub,
                    PaymentOptions {
                        subscription_id: Some(guid.into()),
                        ..Default::default()
                    },
                    &CancellationToken::new()
                )
                .await
        )
        .http_status,
        503
    );
    start(&buyer).await.wait(fast()).await.unwrap();
    assert_eq!(script.requests.lock().unwrap().len(), 3);
    assert!(
        Buyer::new(ClientOptions {
            authentication: Authentication::ApiKey(String::new()),
            ..Default::default()
        })
        .is_err()
    );
}

#[tokio::test(start_paused = true)]
async fn invalid_challenges_are_rejected_without_network_and_wire_fields_are_preserved() {
    let (buyer, script) = client(vec![]);
    let mut invalid = challenge();
    invalid.id.clear();
    assert!(
        buyer
            .prepare(
                &invalid,
                PaymentOptions::default(),
                &CancellationToken::new()
            )
            .await
            .is_err()
    );
    invalid = challenge();
    invalid.request = Base64UrlJson::from_raw("!");
    assert!(
        buyer
            .prepare(
                &invalid,
                PaymentOptions::default(),
                &CancellationToken::new()
            )
            .await
            .is_err()
    );
    invalid.request = Base64UrlJson::from_raw("e30");
    assert!(
        buyer
            .prepare(
                &invalid,
                PaymentOptions::default(),
                &CancellationToken::new()
            )
            .await
            .is_err()
    );
    invalid = challenge();
    invalid.method = "tempo".into();
    invalid.request = Base64UrlJson::from_raw(encode(&json!({"amount":"1"})).unwrap());
    assert!(
        buyer
            .prepare(
                &invalid,
                PaymentOptions {
                    instrument_id: Some("11111111-1111-1111-1111-111111111111".into()),
                    subscription_id: None
                },
                &CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert!(script.requests.lock().unwrap().is_empty());
    let mut original = challenge();
    original.request = Base64UrlJson::from_raw("eyJjdXJyZW5jeSI6ICJVU0RDIiwgImFtb3VudCI6ICIxIn0=");
    original.description = Some("quoted \"description\"".into());
    original.opaque = Some(Base64UrlJson::from_raw("eyJ4IjogMX0="));
    original.digest = Some("sha-256=:test:".into());
    original.header = Some("Payment-Authorization".into());
    original.expires = Some("2099-01-01T00:00:00Z".into());
    let complete = json!({"challenge":original,"payload":{"proof":"synthetic","extension":true},"source":"did:example:payer"});
    let (buyer, script) = client(vec![reply(
        json!({"state":"ready","credential":encode(&complete).unwrap()}),
    )]);
    let result = buyer
        .prepare(
            &original,
            PaymentOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .wait(fast())
        .await
        .unwrap();
    assert_eq!(serde_json::to_value(result).unwrap(), complete);
    let sent: Value = serde_json::from_slice(&script.requests.lock().unwrap()[0].0.body).unwrap();
    assert_eq!(sent["challenge"], json!(original));
}

#[tokio::test(start_paused = true)]
async fn late_approval_identifier_is_cancelled_and_late_wait_does_not_poll() {
    for value in [json!(false), json!("..")] {
        let (buyer, _) = client(vec![
            reply(json!({"state":"pending","transactionId":"tx"})),
            reply(json!({"state":"pending","transactionId":"tx","approvalId":value})),
        ]);
        assert!(start(&buyer).await.wait(fast()).await.is_err());
    }
    let (buyer, _) = client(vec![reply(
        json!({"state":"pending","transactionId":"tx","approvalId":".."}),
    )]);
    assert!(
        buyer
            .prepare(
                &challenge(),
                PaymentOptions::default(),
                &CancellationToken::new()
            )
            .await
            .is_err()
    );
    let (buyer, script) = client(vec![
        reply(json!({"state":"pending","transactionId":"tx"})),
        reply(pending()),
        reply(json!({"state":"failed"})),
        reply(json!({})),
    ]);
    assert_eq!(
        error(start(&buyer).await.wait(fast()).await).code,
        "MPP_PAYMENT_FAILED"
    );
    assert_eq!(script.requests.lock().unwrap().len(), 4);
    let (buyer, script) = client(vec![reply(pending()), reply(json!({}))]);
    let payment = start(&buyer).await;
    tokio::time::sleep(Duration::from_secs(15)).await;
    assert_eq!(
        error(payment.wait(fast()).await).code,
        "MPP_PAYMENT_TIMEOUT"
    );
    assert_eq!(script.requests.lock().unwrap().len(), 2);
    let (buyer, _) = client(vec![reply(json!({"state":"pending","transactionId":".."}))]);
    assert!(start(&buyer).await.wait(fast()).await.is_err());
}

#[tokio::test(start_paused = true)]
async fn cancelled_creation_stops_without_unknown_approval_cleanup() {
    let (buyer, script) = client(vec![]);
    let token = CancellationToken::new();
    token.cancel();
    assert_eq!(
        error(
            buyer
                .prepare(&challenge(), PaymentOptions::default(), &token)
                .await
        )
        .code,
        "MPP_PAYMENT_CANCELLED"
    );
    assert!(script.requests.lock().unwrap().is_empty());
}
