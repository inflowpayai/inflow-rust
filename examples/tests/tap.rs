use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signer, SigningKey};
use inflow_tap_seller::{KeyResolver, ResolveFuture, Verifier, VerifierOptions};
use sha2::{Digest, Sha256};
use std::sync::Arc;

struct Resolver;
impl KeyResolver for Resolver {
    fn resolve<'a>(&'a self, _: &'a str, _: &'a str) -> ResolveFuture<'a> {
        Box::pin(async {
            Ok(Some(
                SigningKey::from_bytes(&[17; 32]).verifying_key().to_bytes(),
            ))
        })
    }
}
fn verifier() -> Verifier {
    Verifier::new(VerifierOptions {
        key_resolver: Some(Arc::new(Resolver)),
        clock: Some(Arc::new(|| 1800000000000)),
        ..Default::default()
    })
    .unwrap()
}

#[tokio::test]
async fn actual_http_example_requires_tap_and_rejects_replay() {
    let app = inflow_examples::tap_seller::router(verifier(), "https://seller.example").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/api/catalog?q=red%20shoe",
        listener.local_addr().unwrap()
    );
    let (send, receive) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = receive.await;
            })
            .await
            .unwrap()
    });
    let client = reqwest::Client::new();
    assert_eq!(client.get(&url).send().await.unwrap().status(), 401);
    let params = "(\"@method\" \"@authority\" \"@path\" \"@query\");created=1800000000;expires=1800000300;keyid=\"key\";alg=\"ed25519\";nonce=\"once\";tag=\"agent-browser-auth\"";
    let base = format!(
        "\"@method\": GET\n\"@authority\": seller.example\n\"@path\": /api/catalog\n\"@query\": ?q=red%20shoe\n\"@signature-params\": {params}"
    );
    let signature = format!(
        "sig2=:{}:",
        STANDARD.encode(
            SigningKey::from_bytes(&[17; 32])
                .sign(base.as_bytes())
                .to_bytes()
        )
    );
    for expected in [200, 401] {
        let response = client
            .get(&url)
            .header("signature-input", format!("sig2={params}"))
            .header("signature", &signature)
            .header("forwarded", "host=evil.example")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
    for (nonce, body) in [("empty", ""), ("json", "{\"query\":\"shoes\"}")] {
        let digest = format!(
            "sha-256=:{}:",
            STANDARD.encode(Sha256::digest(body.as_bytes()))
        );
        let params = format!(
            "(\"@method\" \"@authority\" \"@path\" \"@query\" \"content-digest\" \"content-type\");created=1800000000;expires=1800000300;keyid=\"key\";alg=\"ed25519\";nonce=\"{nonce}\";tag=\"agent-browser-auth\""
        );
        let base = format!(
            "\"@method\": POST\n\"@authority\": seller.example\n\"@path\": /api/catalog\n\"@query\": ?q=red%20shoe\n\"content-digest\": {digest}\n\"content-type\": application/json\n\"@signature-params\": {params}"
        );
        let signature = STANDARD.encode(
            SigningKey::from_bytes(&[17; 32])
                .sign(base.as_bytes())
                .to_bytes(),
        );
        assert_eq!(
            client
                .post(&url)
                .header("signature-input", format!("sig2={params}"))
                .header("signature", format!("sig2=:{signature}:"))
                .header("content-type", "application/json")
                .header("content-digest", digest)
                .header("content-length", body.len())
                .body(body)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }
    assert_eq!(
        client
            .post(&url)
            .body(vec![0; 1024 * 1024 + 1])
            .send()
            .await
            .unwrap()
            .status(),
        413
    );
    send.send(()).unwrap();
    server.await.unwrap();
}
#[test]
fn rejects_unsafe_public_origins() {
    for url in [
        "bad",
        "https://user:pass@example",
        "https://example/path",
        "https://example?q=1",
        "https://example#f",
        "ftp://example",
    ] {
        assert!(inflow_examples::tap_seller::router(verifier(), url).is_err());
    }
}
