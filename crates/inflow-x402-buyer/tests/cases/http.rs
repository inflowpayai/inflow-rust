use inflow_x402_buyer::*;
use serde_json::{Value, json};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use x402_types::{
    proto,
    scheme::{
        X402SchemeId,
        client::{PaymentCandidate, PaymentCandidateSigner, X402Error},
    },
};

struct Scheme {
    payload: Value,
    calls: Arc<AtomicUsize>,
}
impl X402SchemeId for Scheme {
    fn namespace(&self) -> &str {
        "eip155"
    }
    fn scheme(&self) -> &str {
        "exact"
    }
    fn x402_version(&self) -> u8 {
        2
    }
}
impl X402SchemeClient for Scheme {
    fn accept(&self, _: &proto::PaymentRequired) -> Vec<PaymentCandidate> {
        vec![PaymentCandidate {
            chain_id: "eip155:8453".parse().unwrap(),
            asset: "asset".into(),
            amount: "1".parse().unwrap(),
            scheme: "exact".into(),
            x402_version: 2,
            pay_to: "recipient".into(),
            signer: Box::new(Signer {
                payload: self.payload.clone(),
                calls: self.calls.clone(),
            }),
        }]
    }
}
struct Signer {
    payload: Value,
    calls: Arc<AtomicUsize>,
}
impl PaymentCandidateSigner for Signer {
    fn sign_payment<'life0, 'async_trait>(
        &'life0 self,
    ) -> Pin<Box<dyn Future<Output = Result<String, X402Error>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.payload == json!("fail") {
                return Err(X402Error::SigningError("private signer diagnostic".into()));
            }
            if self.payload == json!("invalid-encoding") {
                return Ok("not base64".into());
            }
            Ok(inflow_x402::encode(&self.payload))
        })
    }
}
struct Reject;
impl PaymentSelector for Reject {
    fn select<'a>(&self, _: &'a [PaymentCandidate]) -> Option<&'a PaymentCandidate> {
        None
    }
}
fn required(declaration: Option<Value>) -> PaymentRequired<OriginalJson> {
    let mut value =
        json!({"x402Version":2,"accepts":[],"resource":{"url":"https://merchant.example"}});
    if let Some(declaration) = declaration {
        value["extensions"] = json!({"payment-identifier":declaration});
    }
    serde_json::from_value(value).unwrap()
}
fn options() -> (SignOptions, WaitOptions, CancellationToken) {
    (
        SignOptions::default(),
        WaitOptions::default(),
        CancellationToken::new(),
    )
}

struct Extension;
impl PaymentExtension for Extension {
    fn enrich<'a>(
        &'a self,
        mut payload: Value,
        _: &'a PaymentRequired<OriginalJson>,
        _: &'a CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Value, Error>> + Send + 'a>> {
        Box::pin(async move {
            if payload == json!("extension-failure") {
                return Err(Error::new("EXTENSION_FAILED", "extension failed"));
            }
            payload["enriched"] = json!(true);
            Ok(payload)
        })
    }
}

#[tokio::test]
async fn extension_preserves_signed_permits_and_does_not_strip_upto_data() {
    for (scheme, entry, retained) in [
        ("exact", json!({"info":{"signature":"0xsigned"}}), true),
        ("exact", json!({"info":{"version":"1"}}), false),
        ("exact", json!({"info":{"signature":""}}), false),
        ("upto", json!({"info":{"version":"1"}}), true),
    ] {
        let http=HttpBuyer::new(None).unwrap().register(Scheme {payload:json!({"accepted":{"scheme":scheme},"extensions":{"eip2612GasSponsoring":entry}}),calls:Arc::new(AtomicUsize::new(0))}).with_extension(Extension);
        let (sign, wait, token) = options();
        let result = http
            .payment(&required(None), sign, wait, &token)
            .await
            .unwrap();
        assert_eq!(
            result.payment_payload["extensions"]
                .get("eip2612GasSponsoring")
                .is_some(),
            retained
        );
        assert_eq!(result.payment_payload["enriched"], true);
    }
}

#[tokio::test]
async fn custom_signer_encoding_and_extension_errors_propagate() {
    for payload in [json!("invalid-encoding"), json!("extension-failure")] {
        let buyer = HttpBuyer::new(None)
            .unwrap()
            .register(Scheme {
                payload,
                calls: Arc::new(AtomicUsize::new(0)),
            })
            .with_extension(Extension);
        let (sign, wait, token) = options();
        assert!(
            buyer
                .payment(&required(None), sign, wait, &token)
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn external_candidates_preserve_fields_apply_identifier_and_honor_selector() {
    let payload = json!({"x402Version":2,"accepted":{},"payload":{"signature":"secret"},"extensions":{"future":1}});
    let calls = Arc::new(AtomicUsize::new(0));
    let http = HttpBuyer::new(None).unwrap().register(Scheme {
        payload: payload.clone(),
        calls: calls.clone(),
    });
    let (sign, wait, token) = options();
    let result = http
        .payment(&required(None), sign.clone(), wait, &token)
        .await
        .unwrap();
    assert_eq!(result.payment_payload, payload);
    assert_eq!(result.transaction_id, None);
    let declaration = inflow_x402::identifier_declaration();
    let result = http
        .payment(
            &required(Some(declaration.clone())),
            sign.clone(),
            wait,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(result.payment_payload["extensions"]["future"], 1);
    assert_eq!(
        result.payment_payload["extensions"]["payment-identifier"]["schema"],
        declaration["schema"]
    );
    assert!(inflow_x402::valid_payment_id(
        result.payment_payload["extensions"]["payment-identifier"]["info"]["id"]
            .as_str()
            .unwrap()
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let error = http
        .with_selector(Reject)
        .payment(&required(None), sign, wait, &token)
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, "X402_NO_MATCH");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn external_failure_cancellation_and_invalid_extension_shapes() {
    let calls = Arc::new(AtomicUsize::new(0));
    let (sign, wait, token) = options();
    let http = HttpBuyer::new(None).unwrap();
    assert_eq!(
        http.payment(&required(None), sign.clone(), wait, &token)
            .await
            .err()
            .unwrap()
            .code,
        "X402_NO_MATCH"
    );
    let http = http.register(Scheme {
        payload: json!("fail"),
        calls: calls.clone(),
    });
    let error = http
        .payment(&required(None), sign.clone(), wait, &token)
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, "X402_SIGNING_FAILED");
    assert!(!error.to_string().contains("private signer"));
    token.cancel();
    assert_eq!(
        http.payment(&required(None), sign.clone(), wait, &token)
            .await
            .err()
            .unwrap()
            .code,
        "X402_APPROVAL_CANCELLED"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    for payload in [Value::Null, json!({"extensions":null})] {
        let http = HttpBuyer::new(None).unwrap().register(Scheme {
            payload,
            calls: calls.clone(),
        });
        assert!(
            http.payment(
                &required(Some(inflow_x402::identifier_declaration())),
                sign.clone(),
                wait,
                &CancellationToken::new()
            )
            .await
            .is_err()
        );
    }
    let http = HttpBuyer::new(None).unwrap().register(Scheme {
        payload: json!({}),
        calls: calls.clone(),
    });
    assert!(
        http.payment(
            &required(Some(json!({"info":{"required":true}}))),
            sign.clone(),
            wait,
            &CancellationToken::new()
        )
        .await
        .is_err()
    );
    assert!(
        http.payment(
            &required(Some(Value::Null)),
            sign.clone(),
            wait,
            &CancellationToken::new()
        )
        .await
        .is_ok()
    );
    let result = http
        .payment(
            &required(Some(inflow_x402::identifier_declaration())),
            sign,
            wait,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(result.payment_payload["extensions"].is_object());
}

#[tokio::test]
async fn redirects_nonpayment_and_already_paid_responses_never_sign() {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    for (status, header, paid) in [
        (200, "", false),
        (302, "Location: http://127.0.0.1:1/stolen\r\n", false),
        (402, "", true),
        (402, "", false),
        (402, "Payment-Required: not-base64\r\n", false),
        (402, "Payment-Required: e30=\r\n", false),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = [0; 4096];
            assert!(socket.read(&mut bytes).await.unwrap() > 0);
            socket.write_all(format!("HTTP/1.1 {status} Result\r\n{header}Content-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        });
        let calls = Arc::new(AtomicUsize::new(0));
        let http = HttpBuyer::new(None).unwrap().register(Scheme {
            payload: json!({}),
            calls: calls.clone(),
        });
        let mut request = reqwest::Client::new().get(url).build().unwrap();
        if paid {
            request
                .headers_mut()
                .insert("payment-signature", "already-signed".parse().unwrap());
        }
        let (sign, wait, token) = options();
        let result = http.execute(request, sign, wait, &token).await;
        if status == 402 && !paid {
            assert!(result.is_err());
        } else {
            assert_eq!(result.unwrap().status().as_u16(), status);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        server.await.unwrap();
    }
    let http = HttpBuyer::new(None).unwrap();
    for url in ["ftp://host/file", "http://name:secret@host/file"] {
        let request = reqwest::Request::new(reqwest::Method::GET, url.parse().unwrap());
        let (sign, wait, token) = options();
        assert!(http.execute(request, sign, wait, &token).await.is_err());
    }
    for cancelled in [true, false] {
        let request =
            reqwest::Request::new(reqwest::Method::GET, "http://127.0.0.1:1/".parse().unwrap());
        let (sign, wait, token) = options();
        if cancelled {
            token.cancel();
        }
        let error = http
            .execute(request, sign, wait, &token)
            .await
            .err()
            .unwrap();
        assert_eq!(
            error.code,
            if cancelled {
                "X402_APPROVAL_CANCELLED"
            } else {
                "X402_RESOURCE_HTTP_ERROR"
            }
        );
    }
}

#[tokio::test]
async fn paid_post_replays_once_without_following_redirects_or_replacing_service_auth() {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    for paid_status in [200, 307, 402] {
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let destination = format!("http://{}/other", target.local_addr().unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/resource", listener.local_addr().unwrap());
        let challenge = inflow_x402::encode(&serde_json::to_value(required(None)).unwrap());
        let server = tokio::spawn(async move {
            for step in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                while !bytes.ends_with(b"\r\n\r\n") {
                    bytes.push(stream.read_u8().await.unwrap());
                }
                let headers = String::from_utf8(bytes).unwrap();
                assert!(headers.starts_with("POST /resource HTTP/1.1"));
                let header = |name: &str| {
                    headers.lines().find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case(name).then_some(value.trim())
                    })
                };
                assert_eq!(header("authorization"), Some("Bearer service-session"));
                assert_eq!(header("cookie"), Some("service=session"));
                assert!(header("x-api-key").is_none());
                assert_eq!(header("payment-signature").is_some(), step == 1);
                let length: usize = header("content-length").unwrap().parse().unwrap();
                let mut body = vec![0; length];
                stream.read_exact(&mut body).await.unwrap();
                assert_eq!(body, b"request-body");
                let (status, extra) = if step == 0 {
                    (402, format!("Payment-Required: {challenge}\r\n"))
                } else {
                    (paid_status, format!("Location: {destination}\r\n"))
                };
                stream.write_all(format!("HTTP/1.1 {status} Result\r\n{extra}Content-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
            }
        });
        let calls = Arc::new(AtomicUsize::new(0));
        // Only signing is synthetic; execute uses the real merchant HTTP client and replay path.
        let buyer = HttpBuyer::new(None).unwrap().register(Scheme {
            payload: json!({"payload":{"proof":"synthetic"}}),
            calls: calls.clone(),
        });
        let request = reqwest::Client::new()
            .post(url)
            .bearer_auth("service-session")
            .header("cookie", "service=session")
            .body("request-body")
            .build()
            .unwrap();
        let (sign, wait, token) = options();
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            buyer.execute(request, sign, wait, &token),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.status().as_u16(), paid_status);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        server.await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), target.accept())
                .await
                .is_err()
        );
    }
}
