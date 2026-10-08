use inflow_core::{Transport, TransportError, TransportRequest, TransportResponse};
use inflow_mpp_buyer::*;
use serde_json::Value;
use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

struct Script(Mutex<VecDeque<Value>>);
impl Transport for Script {
    fn send(
        &self,
        request: TransportRequest,
    ) -> Pin<Box<dyn Future<Output = Result<TransportResponse, TransportError>> + Send + '_>> {
        let exchange = self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected HTTP request");
        Box::pin(async move {
            let expected = &exchange["request"];
            assert_eq!(
                request.method.as_str(),
                expected["method"].as_str().unwrap()
            );
            assert_eq!(
                request.url,
                format!(
                    "https://api.inflowpay.ai{}",
                    expected["path"].as_str().unwrap()
                )
            );
            for (name, value) in expected["headers"].as_object().unwrap() {
                assert_eq!(request.headers[name], value.as_str().unwrap());
            }
            if let Some(body) = expected.get("json") {
                assert_eq!(
                    serde_json::from_slice::<Value>(&request.body).unwrap(),
                    *body
                );
            } else {
                assert!(request.body.is_empty());
            }
            let response = &exchange["response"];
            if let Some(delay) = response["delay_ms"].as_u64() {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            Ok(TransportResponse {
                status: response["status"].as_u64().unwrap() as u16,
                headers: Default::default(),
                body: response
                    .get("json")
                    .map(|v| v.to_string().into_bytes())
                    .unwrap_or_default(),
            })
        })
    }
}

#[tokio::test(start_paused = true)]
async fn shared_buyer_cases_use_the_public_lifecycle() {
    // inflow-specs d79cc3ab3b3acde196e41d15783ed3d119f19379; no lifecycle logic in this adapter.
    let cases: Vec<Value> = serde_json::from_str(include_str!("../data/buyer.json")).unwrap();
    assert_eq!(cases.len(), 25);
    for case in cases {
        let input = &case["input"];
        let transport = Arc::new(Script(Mutex::new(
            case["platform"]["exchanges"]
                .as_array()
                .unwrap()
                .clone()
                .into(),
        )));
        let buyer = Buyer::new(ClientOptions {
            authentication: Authentication::ApiKey(input["api_key"].as_str().unwrap().into()),
            transport: Some(transport.clone()),
            ..Default::default()
        })
        .unwrap();
        let challenge = serde_json::from_value(input["challenge"].clone()).unwrap();
        let options = PaymentOptions {
            instrument_id: input["context"]["instrumentId"].as_str().map(str::to_owned),
            subscription_id: input["context"]["subscriptionId"]
                .as_str()
                .map(str::to_owned),
        };
        let outcome = match buyer
            .prepare(&challenge, options, &CancellationToken::new())
            .await
        {
            Ok(payment) => {
                if case["operation"] == "mpp.buyer.cancel" {
                    payment.cancel().await.unwrap();
                }
                payment
                    .wait(WaitOptions {
                        poll_interval: Duration::ZERO,
                        timeout: Duration::from_millis(
                            input["timeout_ms"].as_u64().unwrap_or(5000),
                        ),
                    })
                    .await
            }
            Err(error) => Err(error),
        };
        if let Some(result) = case["expect"].get("result") {
            assert_eq!(
                serde_json::to_value(outcome.unwrap()).unwrap(),
                *result,
                "{}",
                case["id"]
            );
        } else {
            let error = match outcome {
                Err(error) => error,
                Ok(_) => panic!("expected failure {}", case["id"]),
            };
            let name = match error.code.as_str() {
                "MPP_PAYMENT_FAILED" => "payment-failed",
                "MPP_PAYMENT_EXPIRED" => "payment-expired",
                "MPP_PAYMENT_TIMEOUT" => "payment-timeout",
                "MPP_PAYMENT_CANCELLED" => "payment-cancelled",
                "MPP_MALFORMED_CREDENTIAL" => "invalid-credential",
                _ => panic!("unexpected error {error:?}"),
            };
            assert_eq!(
                name,
                case["expect"]["error"]["code"].as_str().unwrap(),
                "{}",
                case["id"]
            );
            if let Some(problem) = case["expect"]["error"]["details"].get("problem") {
                assert_eq!(error.body["problem"], *problem);
            }
            if let Some(id) = case["expect"]["error"]["details"].get("transaction_id") {
                assert_eq!(error.body["transactionId"], *id);
            }
        }
        tokio::task::yield_now().await;
        assert!(
            transport.0.lock().unwrap().is_empty(),
            "unconsumed requests {}",
            case["id"]
        );
    }
}
