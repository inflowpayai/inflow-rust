use inflow_core::{Transport, TransportError, TransportRequest, TransportResponse};
use inflow_mpp::{Base64UrlJson, encode};
use inflow_mpp_buyer::*;
use serde_json::json;
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Notify,
};

// Destination rewriting only: the SDK lifecycle still makes real HTTP requests and reads bodies.
struct Loopback {
    base: String,
    http: reqwest::Client,
}

#[tokio::test]
async fn status_redirect_and_inflight_cancellation_never_visit_an_action_or_cancel_payment() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let destination=TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listener=TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base=format!("http://{}",listener.local_addr().unwrap());
        let target=format!("http://{}/secret",destination.local_addr().unwrap());
        let entered=Arc::new(Notify::new());
        let notify=entered.clone();
        let server=tokio::spawn(async move {
            for step in 0..2 {
                let (mut stream,_)=listener.accept().await.unwrap();
                let mut bytes=Vec::new();
                while !bytes.ends_with(b"\r\n\r\n") {bytes.push(stream.read_u8().await.unwrap());}
                let request=String::from_utf8(bytes).unwrap();
                assert!(request.starts_with("GET /v1/transactions/original "));
                assert!(request.contains("x-api-key: platform-key"));
                if step==0 {stream.write_all(format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: {target}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}").as_bytes()).await.unwrap();}
                else {notify.notify_one();let mut buffer=[0];assert_eq!(stream.read(&mut buffer).await.unwrap(),0);}
            }
            assert!(tokio::time::timeout(Duration::from_millis(50),listener.accept()).await.is_err());
        });
        let buyer=Buyer::new(ClientOptions {
            authentication:Authentication::ApiKey("platform-key".into()),
            transport:Some(Arc::new(Loopback {base,http:reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).retry(reqwest::retry::never()).build().unwrap()})),
            ..Default::default()
        }).unwrap();
        let token=CancellationToken::new();
        assert_eq!(buyer.get_payment_status("original",PaymentStatusOptions::default(),&token).await.unwrap_err().http_status,307);
        let cancel=token.clone();
        let reading=tokio::spawn(async move {buyer.get_payment_status("original",PaymentStatusOptions::default(),&token).await});
        entered.notified().await;
        cancel.cancel();
        assert_eq!(reading.await.unwrap().unwrap_err().code,"CANCELLED");
        server.await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(50),destination.accept()).await.is_err());
    }).await.unwrap();
}

#[tokio::test]
async fn explicit_http_flow_preserves_challenge_and_separates_platform_credentials() {
    tokio::time::timeout(Duration::from_secs(10), explicit_http_flow())
        .await
        .unwrap();
}

async fn explicit_http_flow() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let mut issued = None;
        for step in 0..3 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") {
                bytes.push(stream.read_u8().await.unwrap());
            }
            let headers = String::from_utf8(bytes).unwrap();
            let header = |name: &str| {
                headers.lines().find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case(name).then_some(value.trim())
                })
            };
            let length = header("content-length")
                .map(|v| v.parse::<usize>().unwrap())
                .unwrap_or(0);
            let mut body = vec![0; length];
            stream.read_exact(&mut body).await.unwrap();
            let (status, extra, body) = match step {
                0 => {
                    assert!(headers.starts_with("GET /resource "));
                    assert_eq!(header("authorization"), Some("Bearer service-session"));
                    assert!(header("x-api-key").is_none());
                    let challenge = r#"Payment id="id", realm="shop\u0041", method="inflow", intent="charge", request="eyJhbW91bnQiOiIxIiwiY3VycmVuY3kiOiJVU0RDIn0", description="Display text", opaque="eyJ4IjogMX0=", header="Payment-Authorization""#;
                    (
                        402,
                        format!("WWW-Authenticate: {challenge}\r\n"),
                        String::new(),
                    )
                }
                1 => {
                    assert!(headers.starts_with("POST /v1/transactions/mpp "));
                    assert_eq!(header("x-api-key"), Some("platform-key"));
                    assert!(header("authorization").is_none());
                    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(value["challenge"]["realm"], "shopu0041");
                    assert_eq!(value["challenge"]["description"], "Display text");
                    assert_eq!(value["challenge"]["opaque"], "eyJ4IjogMX0=");
                    let credential = json!({"challenge":value["challenge"],"source":"did:example:payer","payload":{"proof":"synthetic","extension":true}});
                    let encoded = encode(&credential).unwrap();
                    issued = Some(credential);
                    (
                        200,
                        String::new(),
                        json!({"state":"ready","credential":encoded}).to_string(),
                    )
                }
                _ => {
                    assert!(headers.starts_with("GET /resource "));
                    assert_eq!(header("authorization"), Some("Bearer service-session"));
                    assert!(header("x-api-key").is_none());
                    let encoded = header("payment-authorization")
                        .unwrap()
                        .strip_prefix("Payment ")
                        .unwrap();
                    assert_eq!(
                        &inflow_mpp::decode(encoded).unwrap(),
                        issued.as_ref().unwrap()
                    );
                    (200, String::new(), "resource delivered".into())
                }
            };
            stream.write_all(format!("HTTP/1.1 {status} Response\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        }
    });
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let response = http
        .get(format!("{base}/resource"))
        .bearer_auth("service-session")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 402);
    let values: Vec<_> = response
        .headers()
        .get_all("www-authenticate")
        .iter()
        .map(|v| v.to_str().unwrap())
        .collect();
    let challenge = parse_challenges(&values).unwrap().remove(0);
    let buyer = Buyer::new(ClientOptions {
        authentication: Authentication::ApiKey("platform-key".into()),
        transport: Some(Arc::new(Loopback {
            base: base.clone(),
            http: http.clone(),
        })),
        ..Default::default()
    })
    .unwrap();
    let credential = buyer
        .prepare(
            &challenge,
            PaymentOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .wait(WaitOptions::default())
        .await
        .unwrap();
    let response = http
        .get(format!("{base}/resource"))
        .bearer_auth("service-session")
        .header(
            credential
                .challenge
                .header
                .as_deref()
                .unwrap_or("Authorization"),
            format!("Payment {}", encode_credential(&credential).unwrap()),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.text().await.unwrap(), "resource delivered");
    server.await.unwrap();
}
impl Transport for Loopback {
    fn send(
        &self,
        request: TransportRequest,
    ) -> Pin<Box<dyn Future<Output = Result<TransportResponse, TransportError>> + Send + '_>> {
        Box::pin(async move {
            let url = reqwest::Url::parse(&request.url).unwrap();
            let response = self
                .http
                .request(request.method, format!("{}{}", self.base, url.path()))
                .headers(request.headers)
                .body(request.body)
                .send()
                .await
                .map_err(|_| TransportError::Network)?;
            let status = response.status().as_u16();
            let headers = response.headers().clone();
            let body = response
                .bytes()
                .await
                .map_err(|_| TransportError::Network)?
                .to_vec();
            Ok(TransportResponse {
                status,
                headers,
                body,
            })
        })
    }
}

#[tokio::test]
async fn actual_http_cancellation_during_creation_polling_and_authorization() {
    tokio::time::timeout(Duration::from_secs(15), actual_http_cancellation())
        .await
        .unwrap();
}

async fn actual_http_cancellation() {
    for phase in 0..3 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let started = Arc::new(Notify::new());
        let signal = started.clone();
        let mut challenge = PaymentChallenge::new(
            "id",
            "shop",
            "inflow",
            "charge",
            Base64UrlJson::from_raw(encode(&json!({"amount":"1","currency":"USDC"})).unwrap()),
        );
        let credential = encode(&json!({"challenge":challenge,"payload":{}})).unwrap();
        let server = tokio::spawn(async move {
            let paths = match phase {
                1 => vec![
                    "/v1/transactions/mpp",
                    "/v1/transactions/tx/mpp",
                    "/v1/approvals/approval/cancel",
                    "/v1/transactions/mpp",
                ],
                2 => vec![
                    "/v1/subscriptions/11111111-1111-1111-1111-111111111111/authorize",
                    "/v1/transactions/mpp",
                ],
                _ => vec!["/v1/transactions/mpp", "/v1/transactions/mpp"],
            };
            let mut held = None;
            for (i, path) in paths.iter().enumerate() {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                while !bytes.ends_with(b"\r\n\r\n") {
                    bytes.push(stream.read_u8().await.unwrap());
                }
                let headers = String::from_utf8(bytes).unwrap();
                assert!(headers.lines().next().unwrap().contains(path));
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.split_once(':')
                            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                            .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                let mut body = vec![0; length];
                stream.read_exact(&mut body).await.unwrap();
                if (phase == 1 && i == 1) || (phase != 1 && i == 0) {
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n{")
                        .await
                        .unwrap();
                    held = Some(stream);
                    signal.notify_one();
                    continue;
                }
                let body=if phase==1 && i==0 {json!({"state":"pending","transactionId":"tx","approvalId":"approval","retryAfterSeconds":0})}
                        else if path.contains("/cancel") {json!({})}
                        else {json!({"state":"ready","credential":credential})}.to_string();
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            }
            drop(held);
        });
        let buyer = Buyer::new(ClientOptions {
            transport: Some(Arc::new(Loopback {
                base,
                http: reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .unwrap(),
            })),
            ..Default::default()
        })
        .unwrap();
        let mut options = PaymentOptions::default();
        if phase == 2 {
            challenge.intent = "subscription".into();
            challenge.request=Base64UrlJson::from_raw(encode(&json!({"amount":"1","currency":"USDC","periodUnit":"month","periodCount":1,"subscriptionExpires":"2099-01-01T00:00:00Z"})).unwrap());
            options.subscription_id = Some("11111111-1111-1111-1111-111111111111".into());
        }
        let token = CancellationToken::new();
        let operation = async {
            buyer
                .prepare(&challenge, options, &token)
                .await?
                .wait(WaitOptions::default())
                .await
        };
        let (outcome, ()) = tokio::join!(operation, async {
            started.notified().await;
            token.cancel();
        });
        let error = match outcome {
            Err(e) => e,
            Ok(_) => panic!("expected cancellation"),
        };
        assert_eq!(error.code, "MPP_PAYMENT_CANCELLED");
        let retry = PaymentChallenge::new(
            "id",
            "shop",
            "inflow",
            "charge",
            Base64UrlJson::from_raw(encode(&json!({"amount":"1","currency":"USDC"})).unwrap()),
        );
        buyer
            .prepare(&retry, PaymentOptions::default(), &CancellationToken::new())
            .await
            .unwrap()
            .wait(WaitOptions::default())
            .await
            .unwrap();
        server.await.unwrap();
    }
}
