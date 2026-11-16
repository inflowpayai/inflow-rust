use inflow_core::{Transport, TransportError, TransportRequest, TransportResponse};
use inflow_x402_seller::*;
use serde_json::{Value, json};
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};

#[derive(Clone)]
struct Reply {
    header: Option<String>,
    status: u16,
    valid: bool,
}

impl Transport for Reply {
    fn send(
        &self,
        request: TransportRequest,
    ) -> Pin<Box<dyn Future<Output = Result<TransportResponse, TransportError>> + Send + '_>> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            let input: Value = serde_json::from_slice(&request.body).unwrap();
            let id = input["paymentPayload"]["payload"]["transactionId"].clone();
            let body = if request.url.ends_with("/verify") {
                json!({"isValid":self.valid,"invalidReason":"permit2_allowance_required","payer":id})
            } else {
                json!({"success":self.valid,"transaction":id,"network":"inflow:1"})
            };
            let mut headers = Default::default();
            if let Some(header) = &self.header {
                let mut map = http::HeaderMap::new();
                map.insert("extension-responses", header.parse().unwrap());
                headers = map;
            }
            Ok(TransportResponse {
                status: self.status,
                headers,
                body: serde_json::to_vec(&body).unwrap(),
            })
        })
    }
}

fn request(id: &str) -> VerifyRequest {
    inflow_x402::facilitator_request(
        &json!({"x402Version":2,"accepted":{},"payload":{"transactionId":id}}),
        &json!({}),
    )
    .unwrap()
}

fn client(header: Option<String>, status: u16, valid: bool) -> Facilitator {
    Facilitator::new(ClientOptions {
        transport: Some(Arc::new(Reply {
            header,
            status,
            valid,
        })),
        ..Default::default()
    })
    .unwrap()
}

#[tokio::test]
async fn metadata_is_optional_and_separate_from_payment_results() {
    for (header, expected) in [
        (None, None),
        (Some("not base64".into()), None),
        (Some(inflow_x402::encode(&json!([]))), None),
        (Some(inflow_x402::encode(&json!(null))), None),
        (Some(inflow_x402::encode(&json!({}))), Some(json!({}))),
        (
            Some(inflow_x402::encode(&json!({"extension":{"result":"ok"}}))),
            Some(json!({"extension":{"result":"ok"}})),
        ),
    ] {
        for valid in [true, false] {
            let facilitator = client(header.clone(), 200, valid);
            let token = CancellationToken::new();
            let verification = facilitator
                .verify_with_metadata(&request("one"), &token)
                .await
                .unwrap();
            let settlement = facilitator
                .settle_with_metadata(&request("one"), &token)
                .await
                .unwrap();
            assert_eq!(
                verification.extension_responses.map(Value::Object),
                expected
            );
            assert_eq!(settlement.extension_responses.map(Value::Object), expected);
            assert_eq!(verification.response.0["isValid"], valid);
            assert_eq!(settlement.response.0["success"], valid);
            assert!(
                serde_json::to_value(settlement.response)
                    .unwrap()
                    .get("extensionResponses")
                    .is_none()
            );
        }
    }
}

#[tokio::test]
async fn normalized_allowance_response_retains_metadata_but_http_errors_do_not_call_observers() {
    let seen = Arc::new(Mutex::new(0));
    for status in [412, 403] {
        let seen = seen.clone();
        let facilitator = client(
            Some(inflow_x402::encode(&json!({"allowance":{}}))),
            status,
            false,
        )
        .with_response_handler(move |_, _| *seen.lock().unwrap() += 1);
        let result = facilitator
            .verify_with_metadata(&request("one"), &CancellationToken::new())
            .await;
        if status == 412 {
            assert_eq!(
                result.unwrap().extension_responses.unwrap()["allowance"],
                json!({})
            );
        } else {
            assert_eq!(result.unwrap_err().http_status, 403);
        }
    }
    assert_eq!(*seen.lock().unwrap(), 1);
}

#[tokio::test]
async fn native_calls_report_their_own_request_and_never_serialize_metadata() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let observations = seen.clone();
    let facilitator = client(
        Some(inflow_x402::encode(&json!({"private":"seller only"}))),
        200,
        true,
    )
    .with_response_handler(move |request, response| {
        let request: Value = serde_json::from_str(request.as_str()).unwrap();
        let id = &request["paymentPayload"]["payload"]["transactionId"];
        let (kind, actual, metadata) = match response {
            FacilitatorResponse::Verify(r) => {
                ("verify", &r.response.0["payer"], &r.extension_responses)
            }
            FacilitatorResponse::Settle(r) => (
                "settle",
                &r.response.0["transaction"],
                &r.extension_responses,
            ),
        };
        assert_eq!(actual, id);
        assert_eq!(metadata.as_ref().unwrap()["private"], "seller only");
        observations.lock().unwrap().push((kind, id.clone()));
    });
    let first = request("first");
    let second = request("second");
    let (verify, settle) = tokio::join!(
        x402_types::facilitator::Facilitator::verify(&facilitator, &first),
        x402_types::facilitator::Facilitator::settle(&facilitator, &second),
    );
    assert!(
        serde_json::to_value(verify.unwrap())
            .unwrap()
            .get("extensionResponses")
            .is_none()
    );
    assert!(
        serde_json::to_value(settle.unwrap())
            .unwrap()
            .get("extensionResponses")
            .is_none()
    );
    assert_eq!(seen.lock().unwrap().len(), 2);
}
