use super::*;
use ::http::{Request, Response};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use mpp::server::middleware::PaymentVerifier;
use std::convert::Infallible;
use tower::{Layer, ServiceExt, service_fn};

fn header(offer: &Offer, body: Option<&[u8]>) -> String {
    format!(
        "Payment {}",
        inflow_mpp::encode_credential(&credential(offer, body)).unwrap()
    )
}

fn problem(response: &Response<Vec<u8>>, status: u16, challenged: bool) -> Value {
    assert_eq!(response.status(), status);
    assert_eq!(
        response.headers()["content-type"],
        "application/problem+json"
    );
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(
        response.headers().contains_key("www-authenticate"),
        challenged
    );
    assert!(!response.headers().contains_key("payment-receipt"));
    let problem: Value = serde_json::from_slice(response.body()).unwrap();
    assert_eq!(problem["status"], status);
    assert_eq!(problem.get("challengeId").is_some(), challenged);
    problem
}

#[tokio::test]
async fn real_layer_settles_before_handler_and_preserves_application_response() {
    for status in [200, 503] {
        let (seller, script) = seller(vec![
            Reply::Config(config()),
            Reply::Validate(json!({})),
            Reply::Receipt(receipt()),
        ])
        .await;
        let offer = offer(&seller);
        let credential = header(&offer, None);
        let observed = script.clone();
        let app = service_fn(move |_: Request<()>| {
            let requests = observed.requests.lock().unwrap();
            assert_eq!(requests.len(), 3);
            assert!(requests[1].url.ends_with("/validate"));
            assert!(requests[2].url.ends_with("/broadcast"));
            async move {
                Ok::<_, Infallible>(
                    Response::builder()
                        .status(status)
                        .header("x-application", "retained")
                        .body(b"application body".to_vec())
                        .unwrap(),
                )
            }
        });
        let layer =
            crate::PaymentLayer::new(offer).with_problem_details(|bytes: Bytes| bytes.to_vec());
        let response = layer
            .layer(app)
            .oneshot(
                Request::builder()
                    .header("authorization", credential)
                    .body(())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        assert_eq!(response.body(), b"application body");
        assert_eq!(response.headers()["x-application"], "retained");
        assert!(!response.headers().contains_key("www-authenticate"));
        if status == 200 {
            let receipt =
                inflow_mpp::decode_receipt(response.headers()["payment-receipt"].to_str().unwrap())
                    .unwrap();
            assert_eq!(receipt.reference, "tx");
            assert_eq!(response.headers()["cache-control"], "private");
        } else {
            // Upstream preserves application failures; it does not issue a replacement challenge or retry settlement.
            assert!(!response.headers().contains_key("payment-receipt"));
        }
        assert_eq!(script.requests.lock().unwrap().len(), 3);
    }
}

#[tokio::test]
async fn real_layer_challenges_missing_bad_and_wrong_offer_credentials_without_platform_calls() {
    for case in ["missing", "invalid", "wrong-offer", "wrong-header"] {
        let (seller, script) = seller(vec![Reply::Config(config())]).await;
        let mut offer = offer(&seller);
        offer.options.requires_auth = true;
        let mut request = Request::builder().header("authorization", "Bearer application-token");
        match case {
            "invalid" => request = request.header("payment-authorization", "Payment invalid"),
            "wrong-offer" => {
                let other = seller
                    .offer(
                        Method::Inflow,
                        json!({"amount":"2.00","currency":"USDC"}),
                        offer.options.clone(),
                    )
                    .unwrap();
                request = request.header("payment-authorization", header(&other, None));
            }
            "wrong-header" => {
                request = Request::builder().header("authorization", header(&offer, None))
            }
            _ => {}
        }
        let app = service_fn(|_: Request<()>| async {
            Err::<Response<Vec<u8>>, _>("unpaid handler ran")
        });
        let response = crate::PaymentLayer::new(offer)
            .with_problem_details(|bytes: Bytes| bytes.to_vec())
            .layer(app)
            .oneshot(request.body(()).unwrap())
            .await
            .unwrap();
        problem(&response, 402, true);
        assert!(
            response.headers()["www-authenticate"]
                .to_str()
                .unwrap()
                .contains("Payment-Authorization")
        );
        assert_eq!(script.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn real_layer_rejections_and_uncertainty_never_deliver_or_expose_backend_errors() {
    for (status, expected, challenged) in [(402, 402, true), (403, 403, false), (500, 500, false)] {
        for during_settlement in [false, true] {
            let mut replies = vec![Reply::Config(config())];
            let failure = json!({"type":"https://paymentauth.org/problems/verification-failed", "status":status,"detail":"private-backend-detail"});
            if during_settlement {
                replies.push(Reply::Validate(json!({})));
                replies.push(Reply::Receipt(json!({"problem":failure})));
            } else {
                replies.push(Reply::Validate(json!({"success":false,"problem":failure})));
            }
            let (seller, _) = seller(replies).await;
            let offer = offer(&seller);
            let credential = header(&offer, None);
            let app = service_fn(|_: Request<()>| async {
                Err::<Response<Vec<u8>>, _>("failed payment delivered")
            });
            let response = crate::PaymentLayer::new(offer)
                .with_problem_details(|bytes: Bytes| bytes.to_vec())
                .layer(app)
                .oneshot(
                    Request::builder()
                        .header("authorization", credential)
                        .body(())
                        .unwrap(),
                )
                .await
                .unwrap();
            problem(&response, expected, challenged);
            assert!(!String::from_utf8_lossy(response.body()).contains("private-backend-detail"));
        }
    }
}

#[tokio::test]
async fn real_body_layer_binds_and_delivers_exact_bytes_and_preserves_login_header() {
    for case in ["missing", "changed", "too-large", "valid"] {
        let mut replies = vec![Reply::Config(config())];
        if case == "valid" {
            replies.extend([Reply::Validate(json!({})), Reply::Receipt(receipt())]);
        }
        let (seller, script) = seller(replies).await;
        let mut offer = offer(&seller);
        offer.options.requires_auth = true;
        let credential = header(&offer, Some(b"query"));
        let observed = script.clone();
        let app = service_fn(move |request: Request<Full<Bytes>>| {
            assert_eq!(observed.requests.lock().unwrap().len(), 3);
            assert_eq!(
                request.headers()["authorization"],
                "Bearer application-token"
            );
            async move {
                assert_eq!(
                    request.into_body().collect().await.unwrap().to_bytes(),
                    "query"
                );
                Ok::<_, Infallible>(Response::new(b"result".to_vec()))
            }
        });
        let mut request = Request::builder().header("authorization", "Bearer application-token");
        if case != "missing" {
            request = request.header("payment-authorization", credential);
        }
        let bytes = if case == "changed" {
            b"other".as_slice()
        } else {
            b"query".as_slice()
        };
        let response = crate::PaymentBodyLayer::new(offer)
            .max_body_bytes(if case == "too-large" { 4 } else { 10 })
            .with_problem_details(|bytes: Bytes| bytes.to_vec())
            .layer(app)
            .oneshot(
                request
                    .body(Full::new(Bytes::copy_from_slice(bytes)))
                    .unwrap(),
            )
            .await
            .unwrap();
        if case == "valid" {
            assert_eq!(response.status(), 200);
            assert_eq!(response.body(), b"result");
        } else {
            problem(
                &response,
                if case == "too-large" { 413 } else { 402 },
                case != "too-large",
            );
            assert_eq!(script.requests.lock().unwrap().len(), 1);
        }
    }
}

#[tokio::test]
async fn verifier_string_methods_use_the_same_settlement_and_body_checks() {
    let (seller, _) = seller(vec![
        Reply::Config(config()),
        Reply::Validate(json!({})),
        Reply::Receipt(receipt()),
        Reply::Validate(json!({})),
        Reply::Receipt(receipt()),
    ])
    .await;
    let offer = offer(&seller);
    assert!(
        PaymentVerifier::verify(&offer, &header(&offer, None))
            .await
            .is_ok()
    );
    assert!(
        PaymentVerifier::verify_with_body(&offer, &header(&offer, Some(b"query")), b"query")
            .await
            .is_ok()
    );
    for invalid in ["Bearer abc", "Payment invalid"] {
        assert!(PaymentVerifier::verify(&offer, invalid).await.is_err());
        assert!(
            PaymentVerifier::verify_with_body(&offer, invalid, b"query")
                .await
                .is_err()
        );
    }
}

#[tokio::test(start_paused = true)]
async fn transport_loss_and_malformed_settlement_do_not_issue_another_challenge() {
    for malformed in [false, true] {
        let mut replies = vec![Reply::Config(config()), Reply::Validate(json!({}))];
        if malformed {
            replies.push(Reply::Receipt(json!({"receipt":{}})));
        } else {
            replies.extend([
                Reply::Network,
                Reply::Network,
                Reply::Network,
                Reply::Network,
            ]);
        }
        let (seller, script) = seller(replies).await;
        let offer = offer(&seller);
        let credential = header(&offer, None);
        let app = service_fn(|_: Request<()>| async {
            Err::<Response<Vec<u8>>, _>("uncertain payment delivered")
        });
        let response = crate::PaymentLayer::new(offer)
            .with_problem_details(|bytes: Bytes| bytes.to_vec())
            .layer(app)
            .oneshot(
                Request::builder()
                    .header("authorization", credential)
                    .body(())
                    .unwrap(),
            )
            .await
            .unwrap();
        problem(&response, 500, false);
        let calls = script.requests.lock().unwrap();
        let broadcasts: Vec<_> = calls
            .iter()
            .filter(|call| call.url.ends_with("/broadcast"))
            .collect();
        assert_eq!(broadcasts.len(), if malformed { 1 } else { 4 });
        for call in &broadcasts {
            assert_eq!(
                call.headers["idempotency-key"],
                broadcasts[0].headers["idempotency-key"]
            );
        }
    }
}

#[tokio::test]
async fn verifier_rejects_invalid_request_before_platform_and_challenge_errors_are_safe() {
    let (seller, script) = seller(vec![Reply::Config(config())]).await;
    let mut offer = offer(&seller);
    let mut invalid = credential(&offer, None);
    invalid.challenge.request = Base64UrlJson::from_value(&json!({"amount":1})).unwrap();
    let header = format!(
        "Payment {}",
        inflow_mpp::encode_credential(&invalid).unwrap()
    );
    assert!(matches!(
        PaymentVerifier::verify_credential(&offer, &header, None).await,
        Err(mpp::MppError::MalformedCredential(_))
    ));
    offer.options.description = Some("invalid\nheader".into());
    assert!(PaymentVerifier::challenge(&offer).is_err());
    assert!(PaymentVerifier::challenge_with_body(&offer, b"body").is_err());
    assert_eq!(script.requests.lock().unwrap().len(), 1);
}
