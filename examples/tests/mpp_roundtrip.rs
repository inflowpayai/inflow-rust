use axum::{Router, body::Body, response::IntoResponse, routing::get};
use http::{HeaderMap, Request, StatusCode};
use inflow_core::{
    Authentication, ClientOptions, Transport, TransportError, TransportRequest, TransportResponse,
};
use inflow_mpp_buyer::{Buyer, PaymentOptions, WaitOptions};
use inflow_mpp_seller::{Method, PaymentLayer, Seller, SellerOptions};
use serde_json::{Value, json};
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

#[derive(Default)]
struct State {
    events: Vec<&'static str>,
    issued: Value,
    receipt: Value,
}

struct Platform {
    scenario: &'static str,
    state: Mutex<State>,
}

// Only the remote payment service is simulated; the buyer, seller and Tower layer run normally.
impl Transport for Platform {
    fn send(
        &self,
        request: TransportRequest,
    ) -> Pin<Box<dyn Future<Output = Result<TransportResponse, TransportError>> + Send + '_>> {
        Box::pin(async move {
            assert_eq!(request.headers["x-api-key"], "synthetic-key");
            let path = url::Url::parse(&request.url).unwrap().path().to_owned();
            let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            let mut state = self.state.lock().unwrap();
            let result = match path.as_str() {
                "/v1/mpp/config" => json!({
                    "sellerId":"11111111-1111-4111-8111-111111111111",
                    "featureFlags":{"idempotencyKeyEnabled":true},
                    "supportedMethods":[{"id":"inflow","methodDetails":{
                        "currencyRails":{"USDC":{"rail":"balance"}},
                        "intentCurrencyRails":{"charge":{"USDC":[{"rail":"balance"}]}}
                    }}]
                }),
                "/v1/transactions/mpp" => {
                    state.events.push("create");
                    state.issued = json!({"challenge":body["challenge"], "source":"did:inflow:buyer", "payload":{"transactionId":"22222222-2222-4222-8222-222222222222"}});
                    json!({"state":"ready","credential":inflow_mpp::encode(&state.issued).unwrap()})
                }
                "/v1/mpp/validate" => {
                    state.events.push("validate");
                    assert_eq!(body["credential"], state.issued);
                    if self.scenario == "validation-rejected" {
                        json!({"success":false,"problem":{"status":402,"title":"Rejected"}})
                    } else {
                        let c = &body["credential"];
                        json!({"success":true,"credential":c,"challenge":c["challenge"],"method":"inflow","intent":"charge","source":c["source"],"request":inflow_mpp::decode(c["challenge"]["request"].as_str().unwrap()).unwrap(),"details":{}})
                    }
                }
                "/v1/mpp/broadcast" => {
                    state.events.push("broadcast");
                    assert_eq!(body["credential"], state.issued);
                    if self.scenario == "pending" {
                        json!({"problem":{"status":503,"title":"Settlement pending"}})
                    } else {
                        state.receipt = json!({"method":"inflow","challengeId":body["credential"]["challenge"]["id"],"reference":"synthetic-settlement","status":"success","timestamp":"2026-01-01T00:00:00Z"});
                        json!({"receipt":state.receipt})
                    }
                }
                _ => panic!("unexpected platform request: {path}"),
            };
            Ok(TransportResponse {
                status: 200,
                headers: HeaderMap::new(),
                body: serde_json::to_vec(&result).unwrap(),
            })
        })
    }
}

async fn roundtrip(scenario: &'static str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        let platform = Arc::new(Platform {
            scenario,
            state: Mutex::new(State::default()),
        });
        let options = ClientOptions {
            authentication: Authentication::ApiKey("synthetic-key".into()),
            transport: Some(platform.clone()),
            ..Default::default()
        };
        let token = CancellationToken::new();
        let seller = Seller::new(
            options.clone(),
            SellerOptions {
                realm: "seller.test".into(),
                secret_key: "synthetic-secret-at-least-32-bytes".into(),
            },
            &token,
        )
        .await
        .unwrap();
        let offer = seller
            .offer(
                Method::Inflow,
                json!({"amount":"1","currency":"USDC"}),
                Default::default(),
            )
            .unwrap();
        let observed = platform.clone();
        let app = Router::new().route(
            "/paid",
            get(move |headers: HeaderMap| {
                let observed = observed.clone();
                async move {
                    assert!(!headers.contains_key("x-api-key"));
                    let mut state = observed.state.lock().unwrap();
                    assert_eq!(state.events, ["create", "validate", "broadcast"]);
                    state.events.push("handler");
                    (
                        if scenario == "handler-failed" {
                            StatusCode::INTERNAL_SERVER_ERROR
                        } else {
                            StatusCode::OK
                        },
                        [("x-application", "retained")],
                        "application body",
                    )
                        .into_response()
                }
            })
            .route_layer(PaymentLayer::new(offer).with_problem_details(Body::from)),
        );
        let response = app
            .clone()
            .oneshot(Request::builder().uri("/paid").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 402);
        assert!(platform.state.lock().unwrap().events.is_empty());
        let challenge = inflow_mpp::parse_challenges(&[response.headers()["www-authenticate"]
            .to_str()
            .unwrap()])
        .unwrap()
        .remove(0);
        let buyer = Buyer::new(options).unwrap();
        let credential = buyer
            .prepare(&challenge, PaymentOptions::default(), &token)
            .await
            .unwrap()
            .wait(WaitOptions::default())
            .await
            .unwrap();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/paid")
                    .header(
                        "authorization",
                        format!(
                            "Payment {}",
                            inflow_mpp::encode_credential(&credential).unwrap()
                        ),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let expected_status = match scenario {
            "success" => 200,
            "validation-rejected" => 402,
            _ => 500,
        };
        assert_eq!(response.status(), expected_status);
        if scenario == "success" {
            assert_eq!(
                inflow_mpp::decode(response.headers()["payment-receipt"].to_str().unwrap())
                    .unwrap(),
                platform.state.lock().unwrap().receipt
            );
        } else {
            // Upstream Tower omits the receipt on application failures, without retrying settlement.
            assert!(!response.headers().contains_key("payment-receipt"));
        }
        if scenario == "success" || scenario == "handler-failed" {
            assert_eq!(response.headers()["x-application"], "retained");
            assert_eq!(
                axum::body::to_bytes(response.into_body(), 1024)
                    .await
                    .unwrap(),
                "application body"
            );
        }
        let expected_events: &[&str] = match scenario {
            "validation-rejected" => &["create", "validate"],
            "pending" => &["create", "validate", "broadcast"],
            _ => &["create", "validate", "broadcast", "handler"],
        };
        assert_eq!(platform.state.lock().unwrap().events, expected_events);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn successful_payment_runs_handler_after_settlement() {
    roundtrip("success").await;
}

#[tokio::test]
async fn rejected_validation_does_not_run_handler() {
    roundtrip("validation-rejected").await;
}

#[tokio::test]
async fn pending_settlement_does_not_run_handler() {
    roundtrip("pending").await;
}

#[tokio::test]
async fn handler_failure_does_not_create_another_payment() {
    roundtrip("handler-failed").await;
}
