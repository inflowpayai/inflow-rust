use super::*;
use ::http::{Request, Response, request::Parts};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use std::{
    convert::Infallible,
    sync::atomic::{AtomicUsize, Ordering},
};
use tower::{Layer, ServiceExt, service_fn};

struct FailedBody;
impl http_body::Body for FailedBody {
    type Data = Bytes;
    type Error = std::io::Error;
    fn poll_frame(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
        std::task::Poll::Ready(Some(Err(std::io::Error::other("private body error"))))
    }
}

fn payment(offer: &Offer, body: Option<&[u8]>) -> String {
    format!(
        "Payment {}",
        inflow_mpp::encode_credential(&credential(offer, body)).unwrap()
    )
}

fn amount(seller: &Seller, value: &str) -> Offer {
    seller
        .offer(
            Method::Inflow,
            json!({"amount":value,"currency":"USDC"}),
            Default::default(),
        )
        .unwrap()
}

#[tokio::test]
async fn request_pricing_is_async_isolated_and_resolved_once_per_request() {
    let (seller, script) = seller(vec![Reply::Config(config())]).await;
    let count = Arc::new(AtomicUsize::new(0));
    let calls = count.clone();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let layer = crate::RequestPaymentLayer::new(move |parts: Parts| {
        let seller = seller.clone();
        let calls = calls.clone();
        let barrier = barrier.clone();
        async move {
            calls.fetch_add(1, Ordering::SeqCst);
            barrier.wait().await;
            assert_eq!(parts.method, "POST");
            assert_eq!(parts.headers["x-test"], "retained");
            Ok(amount(&seller, parts.uri.query().unwrap()))
        }
    });
    let app = layer.clone().layer(service_fn(|_: Request<()>| async {
        Err::<Response<Vec<u8>>, _>("unpaid handler")
    }));
    let request = |price| {
        Request::builder()
            .method("POST")
            .uri(format!("/paid?{price}"))
            .header("x-test", "retained")
            .body(())
            .unwrap()
    };
    let (a, b) = tokio::join!(app.clone().oneshot(request("1")), app.oneshot(request("2")));
    for (response, expected) in [(a, "1"), (b, "2")] {
        let response = response.unwrap();
        assert_eq!(response.status(), 402);
        let challenge = inflow_mpp::parse_challenges(&[response.headers()["www-authenticate"]
            .to_str()
            .unwrap()])
        .unwrap()
        .remove(0);
        let request: Value = challenge.request.decode().unwrap();
        assert_eq!(request["amount"], expected);
    }
    assert_eq!(count.load(Ordering::SeqCst), 2);
    assert_eq!(script.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn metadata_pricing_preserves_body_and_rejects_wrong_amount_before_platform() {
    for (paid_amount, expected) in [("1", 200), ("2", 402), ("1", 500)] {
        let mut replies = vec![Reply::Config(config())];
        if expected == 200 {
            replies.extend([Reply::Validate(json!({})), Reply::Receipt(receipt())]);
        }
        let (seller, script) = seller(replies).await;
        let header = payment(&amount(&seller, paid_amount), None);
        let count = Arc::new(AtomicUsize::new(0));
        let calls = count.clone();
        let layer = crate::RequestPaymentLayer::new(move |_: Parts| {
            calls.fetch_add(1, Ordering::SeqCst);
            let offer = amount(&seller, "1");
            async move {
                if expected == 500 {
                    Err(Error::new("PRICING", "secret"))
                } else {
                    Ok(offer)
                }
            }
        });
        let app = layer.layer(service_fn(|request: Request<Full<Bytes>>| async {
            assert_eq!(
                request.into_body().collect().await.unwrap().to_bytes(),
                "original bytes"
            );
            Ok::<_, Infallible>(Response::new(Vec::<u8>::from(b"paid")))
        }));
        let response = app
            .oneshot(
                Request::builder()
                    .header("authorization", header)
                    .body(Full::new(Bytes::from_static(b"original bytes")))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        assert!(!String::from_utf8_lossy(response.body()).contains("secret"));
        assert_eq!(
            response
                .extensions()
                .get::<Error>()
                .map(|error| (error.code.as_str(), error.message.as_str())),
            (expected == 500).then_some(("PRICING", "secret"))
        );
        assert_eq!(
            response.headers().contains_key("payment-receipt"),
            expected == 200
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(
            script.requests.lock().unwrap().len(),
            if expected == 200 { 3 } else { 1 }
        );
    }
}

#[tokio::test]
async fn pricing_errors_are_private_and_never_challenge_or_call_handler() {
    let app =
        crate::RequestPaymentLayer::new(|_: Parts| async { Err(Error::new("PRICING", "secret")) })
            .layer(service_fn(|_: Request<()>| async {
                Err::<Response<Vec<u8>>, _>("unexpected handler")
            }));
    let response = app.oneshot(Request::new(())).await.unwrap();
    assert_eq!(response.status(), 500);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert!(!response.headers().contains_key("www-authenticate"));
    assert!(!String::from_utf8_lossy(response.body()).contains("secret"));
}

#[tokio::test]
async fn body_pricing_preserves_bytes_and_binds_payment_and_limits_before_resolution() {
    for (case, expected) in [
        ("missing", 402),
        ("paid", 200),
        ("wrong-body", 402),
        ("large", 413),
        ("error", 500),
        ("read-error", 400),
    ] {
        let mut replies = vec![Reply::Config(config())];
        if case == "paid" {
            replies.extend([Reply::Validate(json!({})), Reply::Receipt(receipt())]);
        }
        let (seller, script) = seller(replies).await;
        let header = payment(
            &amount(&seller, "1"),
            Some(if case == "wrong-body" { b"other" } else { b"1" }),
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let layer = crate::RequestPaymentBodyLayer::new(move |parts: Parts, body: Bytes| {
            count.fetch_add(1, Ordering::SeqCst);
            assert_eq!(parts.headers["x-test"], "retained");
            let offer = amount(&seller, std::str::from_utf8(&body).unwrap());
            async move {
                if case == "error" {
                    Err(Error::new("PRICING", "secret"))
                } else {
                    Ok(offer)
                }
            }
        })
        .max_body_bytes(4);
        let app = layer.clone().layer(service_fn(
            move |request: Request<Full<Bytes>>| async move {
                assert_eq!(case, "paid");
                assert_eq!(request.into_body().collect().await.unwrap().to_bytes(), "1");
                Ok::<_, Infallible>(Response::new(Vec::<u8>::from(b"paid")))
            },
        ));
        let mut request = Request::builder().header("x-test", "retained");
        if case != "missing" {
            request = request.header("authorization", header);
        }
        let body = if case == "read-error" {
            FailedBody.boxed()
        } else {
            Full::new(Bytes::from_static(if case == "large" {
                b"12345"
            } else {
                b"1"
            }))
            .map_err(|never| match never {})
            .boxed()
        };
        let response = app
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        assert!(!String::from_utf8_lossy(response.body()).contains("secret"));
        assert_eq!(
            response
                .extensions()
                .get::<Error>()
                .map(|error| (error.code.as_str(), error.message.as_str())),
            (case == "error").then_some(("PRICING", "secret"))
        );
        assert_eq!(
            response.headers().contains_key("payment-receipt"),
            case == "paid"
        );
        assert_eq!(
            response.headers().contains_key("www-authenticate"),
            expected == 402
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            usize::from(!matches!(case, "large" | "read-error"))
        );
        assert_eq!(
            script.requests.lock().unwrap().len(),
            if case == "paid" { 3 } else { 1 }
        );
    }
}
