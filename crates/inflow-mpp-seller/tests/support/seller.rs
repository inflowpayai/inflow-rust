use super::*;
use inflow_core::{Authentication, Transport, TransportError, TransportRequest, TransportResponse};
use std::{collections::VecDeque, future::Future, pin::Pin};

mod cards;
mod http;

#[tokio::test]
async fn shared_preparation_and_signed_route_vectors() {
    // inflow-specs d79cc3ab3b3acde196e41d15783ed3d119f19379; these cases mint real local challenges.
    let cases: Vec<Value> = serde_json::from_str(include_str!("../data/route-cases.json")).unwrap();
    assert_eq!(cases.len(), 11);
    for case in cases {
        let configuration = case["platform"]["exchanges"][0]["response"]["json"].clone();
        let (seller, script) = seller(vec![Reply::Config(configuration)]).await;
        let input = &case["input"];
        let method = match input["method"].as_str().unwrap() {
            "inflow" => Method::Inflow,
            "tempo" => Method::Tempo,
            other => panic!("unexpected method {other}"),
        };
        let prepared = seller.offer(method, input["request"].clone(), Default::default());
        if case["operation"] == "mpp.seller.route-binding" {
            let original = prepared.unwrap();
            let replacement = seller
                .offer(
                    method,
                    input["replacement_request"].clone(),
                    Default::default(),
                )
                .unwrap();
            let c = Credential {
                challenge: original.challenge(None).unwrap(),
                payload: input["credential_payload"].as_object().unwrap().clone(),
                source: input["source"].as_str().map(str::to_owned),
            };
            assert_eq!(
                err(replacement
                    .accept(&c, None, &CancellationToken::new())
                    .await)
                .code,
                "MPP_CREDENTIAL_MISMATCH",
                "{}",
                case["id"]
            );
        } else if case["expect"].get("error").is_some() {
            assert!(
                matches!(
                    err(prepared).code.as_str(),
                    "MPP_UNSUPPORTED_CURRENCY"
                        | "MPP_UNSUPPORTED_RAIL"
                        | "MPP_AMBIGUOUS_RAIL"
                        | "MPP_INSTRUMENT_REQUIRED"
                ),
                "{}",
                case["id"]
            );
        } else {
            assert_eq!(
                prepared.unwrap().request(),
                &case["expect"]["result"],
                "{}",
                case["id"]
            );
        }
        assert_eq!(script.requests.lock().unwrap().len(), 1);
    }
}

const SELLER: &str = "11111111-1111-4111-8111-111111111111";
const INSTRUMENT: &str = "55555555-5555-4555-8555-555555555555";

enum Reply {
    Config(Value),
    Validate(Value),
    Receipt(Value),
    Status(u16),
    Network,
    Hang,
}
struct Script {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<TransportRequest>>,
}
impl Transport for Script {
    fn send(
        &self,
        request: TransportRequest,
    ) -> Pin<Box<dyn Future<Output = Result<TransportResponse, TransportError>> + Send + '_>> {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        assert_eq!(request.headers["x-api-key"], "seller-test-key");
        self.requests.lock().unwrap().push(request);
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected request");
        Box::pin(async move {
            let (status, result) = match reply {
                Reply::Config(value) => (200, value),
                Reply::Validate(patch) => {
                    let c = &body["credential"];
                    let mut value = json!({"success":true,"challenge":c["challenge"],"credential":c,"details":{},"method":c["challenge"]["method"],"intent":c["challenge"]["intent"],"request":decode(c["challenge"]["request"].as_str().unwrap()).unwrap(),"source":c["source"]});
                    if c.get("source").is_none() {
                        value.as_object_mut().unwrap().remove("source");
                    }
                    for (key, v) in patch.as_object().unwrap() {
                        if v == "omit-field" {
                            value.as_object_mut().unwrap().remove(key);
                        } else {
                            value[key] = v.clone();
                        }
                    }
                    (200, value)
                }
                Reply::Receipt(value) => (200, value),
                Reply::Status(status) => (status, json!({"code":"TEST_FAILURE"})),
                Reply::Network => return Err(TransportError::Network),
                Reply::Hang => return std::future::pending().await,
            };
            Ok(TransportResponse {
                status,
                headers: Default::default(),
                body: result.to_string().into_bytes(),
            })
        })
    }
}
fn config() -> Value {
    json!({"sellerId":SELLER,"featureFlags":{"idempotencyKeyEnabled":true},"supportedMethods":[{"id":"inflow","methodDetails":{"currencyRails":{"USD":{"rail":"instrument","instrumentId":"optional"},"USDC":{"rail":"balance"}},"intentCurrencyRails":{"charge":{"USD":[{"rail":"instrument","instrumentId":"optional"}],"USDC":[{"rail":"balance"}]}}}}]})
}
fn options() -> SellerOptions {
    SellerOptions {
        realm: "shop".into(),
        secret_key: "local-test-secret-at-least-32-bytes".into(),
    }
}
fn transport(replies: Vec<Reply>) -> (ClientOptions, Arc<Script>) {
    let script = Arc::new(Script {
        replies: Mutex::new(replies.into()),
        requests: Mutex::new(Vec::new()),
    });
    (
        ClientOptions {
            authentication: Authentication::ApiKey("seller-test-key".into()),
            transport: Some(script.clone()),
            ..Default::default()
        },
        script,
    )
}
async fn seller(replies: Vec<Reply>) -> (Seller, Arc<Script>) {
    let (client, script) = transport(replies);
    (
        Seller::new(client, options(), &CancellationToken::new())
            .await
            .unwrap(),
        script,
    )
}
fn offer(seller: &Seller) -> Offer {
    seller
        .offer(
            Method::Inflow,
            json!({"amount":"1.25","currency":"USDC"}),
            ChallengeOptions::default(),
        )
        .unwrap()
}
fn credential(offer: &Offer, body: Option<&[u8]>) -> Credential {
    Credential {
        challenge: offer.challenge(body).unwrap(),
        source: Some("did:example:buyer".into()),
        payload: serde_json::Map::from_iter([("proof".into(), json!("synthetic"))]),
    }
}
fn receipt() -> Value {
    json!({"receipt":{"method":"inflow","reference":"tx","status":"success","timestamp":"2026-09-01T12:00:00Z","settlement":{"amount":"1.25","currency":"USDC"},"extra":{"preserve":true}}})
}

#[tokio::test]
async fn instrument_receipts_must_match_the_paid_challenge() {
    for outcome in ["valid", "method", "identifier", "missing"] {
        let (seller, script) =
            seller(vec![Reply::Config(config()), Reply::Validate(json!({}))]).await;
        let offer = seller
            .offer(
                Method::Inflow,
                json!({"amount":"1.25","currency":"USD"}),
                Default::default(),
            )
            .unwrap();
        let credential = credential(&offer, None);
        let mut result = receipt();
        result["receipt"]["challengeId"] = json!(credential.challenge.id);
        result["receipt"]["settlement"]["currency"] = json!("USD");
        match outcome {
            "method" => result["receipt"]["method"] = json!("tempo"),
            "identifier" => result["receipt"]["challengeId"] = json!("other"),
            "missing" => {
                result["receipt"]
                    .as_object_mut()
                    .unwrap()
                    .remove("challengeId");
            }
            _ => {}
        }
        script
            .replies
            .lock()
            .unwrap()
            .push_back(Reply::Receipt(result));
        let result = offer
            .accept(&credential, None, &CancellationToken::new())
            .await;
        assert_eq!(result.is_ok(), outcome == "valid", "{outcome}");
        assert_eq!(script.requests.lock().unwrap().len(), 3);
    }
}
fn err<T>(result: Result<T, Error>) -> Error {
    match result {
        Err(e) => e,
        Ok(_) => panic!("expected error"),
    }
}

#[tokio::test]
async fn full_credential_and_receipt_survive_upstream_lifecycle() {
    let (seller, script) = seller(vec![
        Reply::Config(config()),
        Reply::Validate(json!({})),
        Reply::Validate(json!({})),
        Reply::Receipt(receipt()),
    ])
    .await;
    let offer = seller
        .offer(
            Method::Inflow,
            json!({"amount":"1.25","currency":"USDC"}),
            ChallengeOptions {
                description: Some("Original description".into()),
                requires_auth: true,
                opaque: Some(Base64UrlJson::from_raw("eyJvcmRlciI6MX0=")),
                expires: Some("2099-01-01T00:00:00Z".into()),
            },
        )
        .unwrap();
    assert_eq!(offer.request()["recipient"], SELLER);
    let credential = credential(&offer, Some(b"actual body"));
    let original = serde_json::to_value(&credential).unwrap();
    let validated = offer
        .validate(&credential, Some(b"actual body"), &CancellationToken::new())
        .await
        .unwrap();
    assert!(validated.success);
    assert_eq!(
        serde_json::to_value(validated.credential).unwrap(),
        original
    );
    assert_eq!(validated.request, *offer.request());
    let receipt = offer
        .accept(&credential, Some(b"actual body"), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(receipt.extensions["extra"], json!({"preserve":true}));
    let requests = script.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    assert!(requests[0].url.ends_with("/config"));
    for request in &requests[1..] {
        assert_eq!(
            serde_json::from_slice::<Value>(&request.body).unwrap()["credential"],
            original
        );
    }
    assert!(requests[1].url.ends_with("/validate"));
    assert!(requests[2].url.ends_with("/validate"));
    assert!(requests[3].url.ends_with("/broadcast"));
    assert!(
        uuid::Uuid::parse_str(requests[3].headers["idempotency-key"].to_str().unwrap()).is_ok()
    );
}

#[tokio::test(start_paused = true)]
async fn config_failures_can_be_retried_and_clones_share_loaded_configuration() {
    let (client, script) = transport(vec![
        Reply::Status(403),
        Reply::Config(config()),
        Reply::Validate(json!({})),
        Reply::Validate(json!({})),
    ]);
    assert_eq!(
        err(Seller::new(client.clone(), options(), &CancellationToken::new()).await).code,
        "TEST_FAILURE"
    );
    let seller = Seller::new(client, options(), &CancellationToken::new())
        .await
        .unwrap();
    let a = offer(&seller);
    let b = offer(&seller.clone());
    let ca = credential(&a, None);
    let cb = credential(&b, None);
    let token = CancellationToken::new();
    let (a, b) = tokio::join!(a.validate(&ca, None, &token), b.validate(&cb, None, &token));
    a.unwrap();
    b.unwrap();
    assert_eq!(script.requests.lock().unwrap().len(), 4);
    for bad in [
        json!({}),
        json!({"sellerId":"","supportedMethods":[]}),
        json!({"sellerId":SELLER,"supportedMethods":{}}),
    ] {
        let (client, _) = transport(vec![Reply::Config(bad)]);
        assert_eq!(
            err(Seller::new(client, options(), &token).await).code,
            "INVALID_MPP_DATA"
        );
    }
    let (client, _) = transport(vec![]);
    for bad in [
        SellerOptions {
            realm: "".into(),
            ..options()
        },
        SellerOptions {
            secret_key: " ".into(),
            ..options()
        },
        SellerOptions {
            secret_key: "a".repeat(31),
            ..options()
        },
    ] {
        assert!(Seller::new(client.clone(), bad, &token).await.is_err());
    }
    let mut bad = client;
    bad.timeout = std::time::Duration::ZERO;
    assert!(Seller::new(bad, options(), &token).await.is_err());
    let (client, _) = transport(vec![Reply::Config(config())]);
    Seller::new(
        client,
        SellerOptions {
            secret_key: "a".repeat(32),
            ..options()
        },
        &token,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn prepare_rails_and_tempo_without_mutating_the_callers_request() {
    let mut c = config();
    c["supportedMethods"][0]["methodDetails"]["intentCurrencyRails"]["charge"]["USDC"] =
        json!([{"rail":"balance"},{"rail":"instrument","instrumentId":"required"}]);
    let (seller, _) = seller(vec![Reply::Config(c)]).await;
    assert_eq!(
        err(seller.offer(
            Method::Inflow,
            json!({"amount":"1","currency":"USDC"}),
            Default::default()
        ))
        .code,
        "MPP_AMBIGUOUS_RAIL"
    );
    assert_eq!(
        err(seller.offer(
            Method::Inflow,
            json!({"amount":"1","currency":"USDC","methodDetails":{"rail":"instrument"}}),
            Default::default()
        ))
        .code,
        "MPP_INSTRUMENT_REQUIRED"
    );
    let input = json!({"amount":"1.25","currency":"USDC","methodDetails":{"rail":"instrument","instrumentId":INSTRUMENT}});
    let prepared = seller
        .offer(Method::Inflow, input.clone(), Default::default())
        .unwrap();
    assert_eq!(prepared.request()["methodDetails"], input["methodDetails"]);
    assert!(input.get("recipient").is_none());
    assert_eq!(
        err(seller.offer(
            Method::Inflow,
            json!({"amount":"1","currency":"USD","methodDetails":{"rail":"balance"}}),
            Default::default()
        ))
        .code,
        "MPP_UNSUPPORTED_RAIL"
    );
    assert_eq!(
        err(seller.offer(
            Method::Inflow,
            json!({"amount":"1","currency":"EUR"}),
            Default::default()
        ))
        .code,
        "MPP_UNSUPPORTED_CURRENCY"
    );
    for input in [
        json!([]),
        json!({}),
        json!({"amount":"x","currency":"USD"}),
        json!({"amount":"1","currency":"USD","methodDetails":false}),
    ] {
        assert!(
            seller
                .offer(Method::Inflow, input, Default::default())
                .is_err()
        );
    }
    assert!(
        seller
            .offer(
                Method::Inflow,
                json!({"amount":"1","currency":"USD"}),
                ChallengeOptions {
                    expires: Some("bad".into()),
                    ..Default::default()
                }
            )
            .is_err()
    );
    let tempo = json!({"amount":"100","currency":"0x1111111111111111111111111111111111111111","recipient":"0x2222222222222222222222222222222222222222"});
    let prepared = seller
        .offer(Method::Tempo, tempo.clone(), Default::default())
        .unwrap();
    assert_eq!(
        prepared.request()["methodDetails"],
        json!({"feePayer":false,"supportedModes":["pull"]})
    );
    let mut explicit = tempo.clone();
    explicit["methodDetails"] = json!({"feePayer":true,"supportedModes":["push"],"chainId":4217});
    assert_eq!(
        seller
            .offer(Method::Tempo, explicit.clone(), Default::default())
            .unwrap()
            .request(),
        &explicit
    );
    for input in [
        json!({"amount":"1"}),
        json!({"amount":"1","currency":"bad","recipient":"bad"}),
        json!({"amount":"1","methodDetails":false}),
    ] {
        assert!(
            seller
                .offer(Method::Tempo, input, Default::default())
                .is_err()
        );
    }
}

#[tokio::test]
async fn legacy_config_and_missing_capabilities() {
    for missing in [false, true] {
        let mut c = config();
        c["supportedMethods"][0]["methodDetails"]
            .as_object_mut()
            .unwrap()
            .remove("intentCurrencyRails");
        if missing {
            c["supportedMethods"] = json!([]);
        }
        let (seller, _) = seller(vec![Reply::Config(c)]).await;
        let result = seller.offer(
            Method::Inflow,
            json!({"amount":"1","currency":"USDC"}),
            Default::default(),
        );
        assert_eq!(result.is_err(), missing);
    }
    let mut c = config();
    c["supportedMethods"][0]["methodDetails"]["intentCurrencyRails"]["charge"]["USDC"] =
        json!([{"rail":"invalid"}]);
    let (seller, _) = seller(vec![Reply::Config(c)]).await;
    assert_eq!(
        err(seller.offer(
            Method::Inflow,
            json!({"amount":"1","currency":"USDC"}),
            Default::default()
        ))
        .code,
        "MPP_UNSUPPORTED_RAIL"
    );
}

#[tokio::test]
async fn bad_provenance_body_and_route_terms_never_reach_platform() {
    let (seller, script) = seller(vec![Reply::Config(config())]).await;
    let a = offer(&seller);
    for (body, actual) in [
        (Some(b"a".as_slice()), Some(b"b".as_slice())),
        (Some(b"a".as_slice()), None),
        (None, Some(b"a".as_slice())),
    ] {
        let c = credential(&a, body);
        assert!(
            a.accept(&c, actual, &CancellationToken::new())
                .await
                .is_err()
        );
    }
    let mut c = credential(&a, None);
    c.challenge.id = "wrong".into();
    assert!(a.accept(&c, None, &CancellationToken::new()).await.is_err());
    let old = seller
        .offer(
            Method::Inflow,
            a.request.clone(),
            ChallengeOptions {
                expires: Some("2000-01-01T00:00:00Z".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        a.accept(&credential(&old, None), None, &CancellationToken::new())
            .await
            .is_err()
    );
    let changed = seller
        .offer(
            Method::Inflow,
            json!({"amount":"2","currency":"USDC"}),
            Default::default(),
        )
        .unwrap();
    assert_eq!(
        err(changed
            .accept(&credential(&a, None), None, &CancellationToken::new())
            .await)
        .code,
        "MPP_CREDENTIAL_MISMATCH"
    );
    let mut malformed = credential(&a, None);
    malformed.challenge.request = Base64UrlJson::from_raw("!");
    assert!(
        a.validate(&malformed, None, &CancellationToken::new())
            .await
            .is_err()
    );
    malformed.challenge.id.clear();
    assert!(
        a.validate(&malformed, None, &CancellationToken::new())
            .await
            .is_err()
    );
    let mut malformed = credential(&a, None);
    malformed.challenge.request = Base64UrlJson::from_raw("e30");
    assert!(
        a.validate(&malformed, None, &CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(script.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn validation_rejects_inconsistent_envelopes_and_preserves_problems() {
    for patch in [
        json!({"success":false,"problem":{"type":"https://paymentauth.org/problems/verification-failed","title":"Payment Verification Failed","status":402,"detail":"denied","hint":"Use another payment method.","extensions":{"reason":"declined"}}}),
        json!({"success":false,"problem":{"detail":"denied","extensions":{"key":"value"}}}),
        json!({"success":false}),
        json!({"challenge":{}}),
        json!({"credential":{}}),
        json!({"source":"other"}),
        json!({"method":"tempo"}),
        json!({"intent":"subscription"}),
        json!({"request":[]}),
        json!({"details":false}),
    ] {
        let (seller, script) = seller(vec![
            Reply::Config(config()),
            Reply::Validate(patch.clone()),
        ])
        .await;
        let offer = offer(&seller);
        let c = credential(&offer, None);
        let error = err(offer.accept(&c, None, &CancellationToken::new()).await);
        assert_eq!(error.code, "MPP_PAYMENT_FAILED");
        if let Some(p) = patch.get("problem") {
            assert_eq!(*error.body, *p);
            assert_eq!(error.to_string(), "denied");
        }
        assert_eq!(script.requests.lock().unwrap().len(), 2);
    }
    let (seller, _) = seller(vec![
        Reply::Config(config()),
        Reply::Validate(json!({"details":"omit-field"})),
    ])
    .await;
    let offer = offer(&seller);
    assert_eq!(
        offer
            .validate(&credential(&offer, None), None, &CancellationToken::new())
            .await
            .unwrap()
            .details,
        json!({})
    );
}

#[tokio::test]
async fn tempo_acceptance_binds_each_transfer_term_before_platform_calls() {
    let (seller, script) = seller(vec![
        Reply::Config(config()),
        Reply::Validate(json!({})),
        Reply::Receipt(receipt()),
    ])
    .await;
    let request = json!({
        "amount":"100000", "currency":"0x1111111111111111111111111111111111111111",
        "recipient":"0x2222222222222222222222222222222222222222",
        "description":"search", "externalId":"order-1",
        "methodDetails":{"chainId":4217,"feePayer":false,"supportedModes":["pull"]}
    });
    let offer = seller
        .offer(Method::Tempo, request.clone(), Default::default())
        .unwrap();
    let original = credential(&offer, None);
    offer
        .accept(&original, None, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(script.requests.lock().unwrap().len(), 3);
    for (field, value) in [
        ("amount", json!("200000")),
        (
            "currency",
            json!("0x3333333333333333333333333333333333333333"),
        ),
        (
            "recipient",
            json!("0x4444444444444444444444444444444444444444"),
        ),
        ("description", json!("other")),
        ("externalId", json!("order-2")),
    ] {
        let mut changed = request.clone();
        changed[field] = value;
        let other = seller
            .offer(Method::Tempo, changed, Default::default())
            .unwrap();
        assert_eq!(
            err(offer
                .accept(&credential(&other, None), None, &CancellationToken::new())
                .await)
            .code,
            "MPP_CREDENTIAL_MISMATCH"
        );
    }
    for (field, value) in [
        ("chainId", json!(42431)),
        ("feePayer", json!(true)),
        ("supportedModes", json!(["push"])),
        ("memo", json!(format!("0x{}", "11".repeat(32)))),
        (
            "splits",
            json!([{"recipient":"0x3333333333333333333333333333333333333333", "amount":"1"}]),
        ),
    ] {
        let mut changed = request.clone();
        changed["methodDetails"][field] = value;
        let other = seller
            .offer(Method::Tempo, changed, Default::default())
            .unwrap();
        assert_eq!(
            err(offer
                .accept(&credential(&other, None), None, &CancellationToken::new())
                .await)
            .code,
            "MPP_CREDENTIAL_MISMATCH"
        );
    }
    assert_eq!(script.requests.lock().unwrap().len(), 3);
    assert_eq!(offer.request()["amount"], "100000");
}

#[tokio::test(start_paused = true)]
async fn broadcast_retries_reuse_key_and_disabled_flag_omits_it() {
    for enabled in [false, true] {
        let mut c = config();
        c["featureFlags"]["idempotencyKeyEnabled"] = json!(enabled);
        let (seller, script) = seller(vec![
            Reply::Config(c),
            Reply::Validate(json!({})),
            Reply::Status(503),
            Reply::Receipt(receipt()),
        ])
        .await;
        let offer = offer(&seller);
        offer
            .accept(&credential(&offer, None), None, &CancellationToken::new())
            .await
            .unwrap();
        let requests = script.requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        assert_eq!(
            requests[2].headers.get("idempotency-key"),
            requests[3].headers.get("idempotency-key")
        );
        assert_eq!(requests[2].headers.contains_key("idempotency-key"), enabled);
        assert_eq!(requests[2].body, requests[3].body);
    }
}

#[tokio::test(start_paused = true)]
async fn terminal_failure_network_failure_and_cancellation_never_become_receipts() {
    for result in [
        json!({}),
        json!({"problem":{"type":"https://paymentauth.org/problems/settlement-unavailable","title":"Settlement Pending","status":503,"detail":"denied","extensions":{"retryAfter":5}}}),
        json!({"problem":{"detail":"denied"}}),
        json!({"receipt":{"status":"failed"}}),
    ] {
        let (seller, _) = seller(vec![
            Reply::Config(config()),
            Reply::Validate(json!({})),
            Reply::Receipt(result.clone()),
        ])
        .await;
        let offer = offer(&seller);
        let error = err(offer
            .accept(&credential(&offer, None), None, &CancellationToken::new())
            .await);
        assert_eq!(error.code, "MPP_PAYMENT_FAILED");
        if let Some(p) = result.get("problem") {
            assert_eq!(*error.body, *p);
            assert_eq!(error.to_string(), "denied");
        }
    }
    for during_broadcast in [false, true] {
        let mut replies = vec![Reply::Config(config())];
        if during_broadcast {
            replies.push(Reply::Validate(json!({})));
        }
        replies.extend([
            Reply::Network,
            Reply::Network,
            Reply::Network,
            Reply::Network,
        ]);
        let (seller, script) = seller(replies).await;
        let offer = offer(&seller);
        assert_eq!(
            err(offer
                .accept(&credential(&offer, None), None, &CancellationToken::new())
                .await)
            .code,
            "NETWORK_ERROR"
        );
        assert_eq!(
            script.requests.lock().unwrap().len(),
            if during_broadcast { 6 } else { 5 }
        );
    }
    let (seller, _) = seller(vec![Reply::Config(config()), Reply::Hang]).await;
    let offer = offer(&seller);
    let c = credential(&offer, None);
    let token = CancellationToken::new();
    let operation = offer.accept(&c, None, &token);
    let cancel = async {
        tokio::task::yield_now().await;
        token.cancel();
    };
    let (result, ()) = tokio::join!(operation, cancel);
    assert_eq!(err(result).code, "CANCELLED");
}

#[tokio::test]
async fn malformed_problem_messages_have_a_stable_fallback() {
    for detail in [Value::Null, json!(""), json!(false)] {
        let (seller, _) = seller(vec![
            Reply::Config(config()),
            Reply::Validate(json!({"success":false,"problem":{"detail":detail}})),
        ])
        .await;
        let offer = offer(&seller);
        let error = err(offer
            .validate(&credential(&offer, None), None, &CancellationToken::new())
            .await);
        assert_eq!(error.code, "MPP_PAYMENT_FAILED");
        assert_eq!(error.to_string(), "payment verification failed");
        assert_eq!(error.body["detail"], detail);
    }
}

#[tokio::test]
async fn upstream_compatibility_verify_uses_both_real_hooks() {
    let (seller, script) = seller(vec![
        Reply::Config(config()),
        Reply::Validate(json!({})),
        Reply::Receipt(receipt()),
    ])
    .await;
    let offer = offer(&seller);
    let c = credential(&offer, None);
    let (bridge, projected, expected) = offer.bridge(&c, &CancellationToken::new()).unwrap();
    bridge.verify(&projected, &expected).await.unwrap();
    assert_eq!(script.requests.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn validation_errors_and_invalid_route_configuration_are_reported_without_settlement() {
    let (seller, script) = seller(vec![
        Reply::Config(config()),
        Reply::Validate(json!({"success":false})),
    ])
    .await;
    assert!(
        seller
            .offer(
                Method::Inflow,
                json!({"amount":"1","currency":"USDC","description":{}}),
                Default::default()
            )
            .is_err()
    );
    let offer = offer(&seller);
    let c = credential(&offer, None);
    assert_eq!(
        err(offer.validate(&c, None, &CancellationToken::new()).await).code,
        "MPP_PAYMENT_FAILED"
    );
    let mut forged = c;
    forged.challenge.id = "forged".into();
    assert_eq!(
        err(offer
            .validate(&forged, None, &CancellationToken::new())
            .await)
        .code,
        "MPP_CREDENTIAL_MISMATCH"
    );
    let unsafe_header = seller
        .offer(
            Method::Inflow,
            json!({"amount":"1","currency":"USDC"}),
            ChallengeOptions {
                description: Some("bad\r\nheader".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(unsafe_header.challenge(None).is_err());
    assert_eq!(script.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn optional_source_is_omitted_not_replaced_with_null() {
    let (seller, _) = seller(vec![
        Reply::Config(config()),
        Reply::Validate(json!({"source":null})),
    ])
    .await;
    let offer = offer(&seller);
    let mut c = credential(&offer, None);
    c.source = None;
    assert_eq!(
        err(offer.validate(&c, None, &CancellationToken::new()).await).code,
        "MPP_PAYMENT_FAILED"
    );
}
