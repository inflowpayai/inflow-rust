use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

const NOW: i64 = 1_800_000_000;
pub(crate) fn key() -> SigningKey {
    SigningKey::from_bytes(&[17; 32])
}
pub(crate) fn signed(body: Option<Vec<u8>>) -> Request {
    let mut components = vec!["@method", "@authority", "@path", "@query"];
    let mut values = vec![
        "GET".to_owned(),
        "merchant.example:8443".into(),
        "/catalog%2Fitems".into(),
        "?q=red%20shoes&kind=a&kind=b".into(),
    ];
    let mut headers = HeaderMap::new();
    if let Some(bytes) = &body {
        components.extend(["content-digest", "content-type"]);
        let digest = format!("sha-256=:{}:", STANDARD.encode(Sha256::digest(bytes)));
        headers.insert("content-digest", digest.parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());
        values.extend([digest, "application/json".into()]);
    }
    let parameters = format!(
        "({});created={NOW};expires={};keyid=\"key\";alg=\"ed25519\";nonce=\"nonce\";tag=\"agent-browser-auth\"",
        components
            .iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(" "),
        NOW + 300
    );
    let mut lines: Vec<_> = components
        .iter()
        .zip(values)
        .map(|(c, v)| format!("\"{c}\": {v}"))
        .collect();
    lines.push(format!("\"@signature-params\": {parameters}"));
    headers.insert(
        "signature-input",
        format!("sig2={parameters}").parse().unwrap(),
    );
    headers.insert(
        "signature",
        format!(
            "sig2=:{}:",
            STANDARD.encode(key().sign(lines.join("\n").as_bytes()).to_bytes())
        )
        .parse()
        .unwrap(),
    );
    Request {
        method: "GET".into(),
        url: "https://merchant.example:8443/catalog%2Fitems?q=red%20shoes&kind=a&kind=b".into(),
        headers,
        body,
    }
}
struct Resolver {
    failure: bool,
    absent: bool,
    bytes: [u8; 32],
    calls: AtomicUsize,
}
impl KeyResolver for Resolver {
    fn resolve<'a>(&'a self, keyid: &'a str, algorithm: &'a str) -> ResolveFuture<'a> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!((keyid, algorithm), ("key", "ed25519"));
            if self.failure {
                return Err(Error::new("CUSTOM", "resolver failed"));
            }
            Ok((!self.absent).then_some(self.bytes))
        })
    }
}
pub(crate) fn verifier() -> Verifier {
    Verifier::new(VerifierOptions {
        key_resolver: Some(Arc::new(Resolver {
            failure: false,
            absent: false,
            bytes: key().verifying_key().to_bytes(),
            calls: AtomicUsize::new(0),
        })),
        clock: Some(Arc::new(|| NOW * 1000)),
        ..Default::default()
    })
    .unwrap()
}

#[tokio::test]
async fn verifies_exact_bytes_and_blocks_replays() {
    for body in [None, Some(vec![]), Some(b"{\"hello\":\"world\"}".to_vec())] {
        let verifier = verifier();
        let request = signed(body);
        let before = request.clone();
        let facts = verifier
            .with_verified(&request, |facts| async move { facts })
            .await
            .unwrap();
        assert_eq!(facts.intent, Intent::Browse);
        assert_eq!(facts.keyid, "key");
        assert_eq!(facts.algorithm, "ed25519");
        assert_eq!(
            verifier.verify(&request).await.unwrap_err().code,
            "NONCE_REPLAYED"
        );
        assert_eq!(request, before);
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_claims_are_atomic() {
    let verifier = verifier();
    let barrier = Arc::new(tokio::sync::Barrier::new(32));
    let tasks = (0..32).map(|_| {
        let verifier = verifier.clone();
        let barrier = barrier.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            verifier.verify(&signed(None)).await
        })
    });
    let results: Vec<_> = futures_util::future::join_all(tasks)
        .await
        .into_iter()
        .map(Result::unwrap)
        .collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert!(
        results
            .iter()
            .filter_map(|r| r.as_ref().err())
            .all(|e| e.code == "NONCE_REPLAYED")
    );
}
#[tokio::test]
async fn tampering_does_not_claim_nonce() {
    for field in ["method", "url", "body", "type", "signature"] {
        let verifier = verifier();
        let original = signed(Some(b"hello".to_vec()));
        let mut request = original.clone();
        match field {
            "method" => request.method = "POST".into(),
            "url" => request.url.push_str("&q=other"),
            "body" => request.body = Some(b"other".to_vec()),
            "type" => {
                request
                    .headers
                    .insert("content-type", "text/plain".parse().unwrap());
            }
            _ => {
                request
                    .headers
                    .insert("signature", "sig2=:AAAA:".parse().unwrap());
            }
        }
        assert!(verifier.verify(&request).await.is_err());
        assert!(verifier.verify(&original).await.is_ok());
    }
}
#[tokio::test]
async fn resolver_and_store_fail_closed() {
    for (failure, absent, bytes, code) in [
        (true, false, key().verifying_key().to_bytes(), "CUSTOM"),
        (false, true, [0; 32], "KEY_NOT_FOUND"),
        (false, false, [0; 32], "SIGNATURE_INVALID"),
        (false, false, [2; 32], "SIGNATURE_INVALID"),
    ] {
        let verifier = Verifier::new(VerifierOptions {
            key_resolver: Some(Arc::new(Resolver {
                failure,
                absent,
                bytes,
                calls: AtomicUsize::new(0),
            })),
            clock: Some(Arc::new(|| NOW * 1000)),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            verifier
                .with_verified(&signed(None), |_| async { panic!("handler must not run") })
                .await
                .unwrap_err()
                .code,
            code
        );
    }
    struct FailedStore;
    impl ReplayStore for FailedStore {
        fn claim<'a>(&'a self, _: &'a str, _: &'a str, _: i64) -> ClaimFuture<'a> {
            Box::pin(async { Err(Error::new("CUSTOM", "store failed")) })
        }
    }
    let mut verifier = verifier();
    verifier.replay = Arc::new(FailedStore);
    assert_eq!(
        verifier.verify(&signed(None)).await.unwrap_err().code,
        "CUSTOM"
    );
}
#[tokio::test]
async fn time_and_memory_lifecycle() {
    for (now, code) in [
        (NOW - 1, "SIGNATURE_NOT_YET_VALID"),
        (NOW + 300, "SIGNATURE_EXPIRED"),
    ] {
        let mut verifier = verifier();
        verifier.clock = Arc::new(move || now * 1000);
        assert_eq!(verifier.verify(&signed(None)).await.unwrap_err().code, code);
    }
    let now = Arc::new(AtomicI64::new(NOW));
    let clock = now.clone();
    let store = MemoryReplayStore::new(Arc::new(move || clock.load(Ordering::SeqCst) * 1000));
    assert!(store.claim("key", "nonce", NOW + 1).await.unwrap());
    assert!(!store.claim("key", "nonce", NOW + 1).await.unwrap());
    now.store(NOW + 1, Ordering::SeqCst);
    assert!(store.claim("key", "nonce", NOW + 10).await.unwrap());
    assert!(
        MemoryReplayStore::default()
            .claim("a", "b", i64::MAX)
            .await
            .unwrap()
    );
    let default = Verifier::new(VerifierOptions::default()).unwrap();
    assert!(
        default
            .verify(&Request {
                headers: HeaderMap::new(),
                ..signed(None)
            })
            .await
            .is_err()
    );
    let error = invalid();
    assert!(!error.to_string().is_empty());
    assert!(std::error::Error::source(&error).is_none());
}
#[test]
fn malformed_inputs_and_parameter_types() {
    let original = signed(None);
    let base = original.headers["signature-input"].to_str().unwrap();
    for input in [
        "",
        "sig1=()",
        "sig2=()",
        "sig2=(\"@method\" \"@method\")",
        "sig2=(\"@method\"x)",
        "sig2=(\"unterminated)",
        "sig2=(\"bad\\x\")",
        "sig2=(\"bad\\",
        "sig2=(\"é\")",
    ] {
        let mut req = original.clone();
        req.headers
            .insert("signature-input", input.parse().unwrap());
        assert!(signature::prepare(&req, NOW).is_err(), "{input}");
    }
    for suffix in [
        ";unknown=1",
        ";created",
        ";created=?0",
        ";created=?1",
        ";created=:AA==:",
        ";created=token",
        ";created=1.2",
        ";created=",
        ";created=-",
        ";created=1x",
        ";created=1234567890123456",
        ";created=1234567890123.1",
        ";created=1.1234",
        ";created=.1",
        ";created=1.",
        ";created=-.1",
        ";created=1.x",
        ";created=?2",
        ";created=:!:",
        ";created=1 2",
        ";keyid=\"\"",
        ";alg=\"RSA\"",
        ";tag=\"other\"",
        ";expires=1",
    ] {
        let mut req = original.clone();
        req.headers.insert(
            "signature-input",
            format!("{base}{suffix}").parse().unwrap(),
        );
        assert!(signature::prepare(&req, NOW).is_err(), "{suffix}");
    }
    for earlier in [
        "?0",
        "?1",
        ":AA==:",
        "token",
        "-1.2",
        "1.2",
        "0001",
        "-0",
        "\"escaped\\\"\\\\value\"",
    ] {
        let mut req = original.clone();
        req.headers.insert(
            "signature-input",
            base.replace(
                &format!(";created={NOW}"),
                &format!(";created={earlier};created={NOW}"),
            )
            .parse()
            .unwrap(),
        );
        assert!(signature::prepare(&req, NOW).is_ok(), "{earlier}");
    }
    for url in [
        "invalid",
        "ftp://example/a",
        "https://user@example/a",
        "https://example/a#f",
    ] {
        let mut req = original.clone();
        req.url = url.into();
        assert!(signature::prepare(&req, NOW).is_err());
    }
    for name in ["signature-input", "signature"] {
        let mut req = original.clone();
        req.headers.remove(name);
        assert!(signature::prepare(&req, NOW).is_err());
        let mut req = original.clone();
        req.headers.append(name, "duplicate".parse().unwrap());
        assert!(signature::prepare(&req, NOW).is_err());
    }
    for value in ["sig1=:AA==:", "sig2=:!:", "sig2=:A:"] {
        let mut req = original.clone();
        req.headers.insert("signature", value.parse().unwrap());
        assert!(signature::prepare(&req, NOW).is_err());
    }
}

#[tokio::test]
async fn raw_headers_and_serialization_boundaries() {
    let original = signed(None);
    let headers = original
        .headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap().to_owned()))
        .collect::<Vec<_>>();
    let converted = Request::new(&original.method, &original.url, headers.clone(), None).unwrap();
    assert_eq!(original, converted);
    assert!(verifier().verify(&converted).await.is_ok());
    for (name, value) in [("bad name", "value"), ("signature-input", "bad\nvalue")] {
        assert_eq!(
            Request::new(
                "GET",
                "https://example/",
                [(name.into(), value.into())],
                None
            )
            .unwrap_err()
            .code,
            "SIGNATURE_INPUT_INVALID"
        );
    }
    let mut headers = headers;
    headers.push(("Signature".into(), "sig2=:AAAA:".into()));
    assert!(
        verifier()
            .verify(&Request::new("GET", &original.url, headers, None).unwrap())
            .await
            .is_err()
    );
    let input = original.headers["signature-input"].to_str().unwrap();
    for field in ["created", "expires", "keyid", "alg", "nonce", "tag"] {
        let prefix = format!(";{field}=");
        let start = input.find(&prefix).unwrap();
        let end = input[start + 1..]
            .find(';')
            .map_or(input.len(), |n| start + 1 + n);
        let mut req = original.clone();
        req.headers.insert(
            "signature-input",
            format!("{}{}", &input[..start], &input[end..])
                .parse()
                .unwrap(),
        );
        assert!(signature::prepare(&req, NOW).is_err());
    }
    let mut req = original.clone();
    req.headers.insert(
        "signature-input",
        input.replace(");", "   );").parse().unwrap(),
    );
    assert_eq!(
        signature::prepare(&req, NOW).unwrap().base,
        signature::prepare(&original, NOW).unwrap().base
    );
    for suffix in [
        ";created=abc!#$%&'*+.^_`|~:/-;created=1800000000",
        ";tag=\"agent-payer-auth\"",
        ";alg=\"Ed25519\"",
    ] {
        let mut req = original.clone();
        req.headers.insert(
            "signature-input",
            format!("{input}{suffix}").parse().unwrap(),
        );
        let parsed = signature::prepare(&req, NOW).unwrap();
        req.headers.insert(
            "signature",
            format!(
                "sig2=:{}:",
                STANDARD.encode(key().sign(parsed.base.as_bytes()).to_bytes())
            )
            .parse()
            .unwrap(),
        );
        assert!(verifier().verify(&req).await.is_ok());
    }
    for input in [
        input.replace("@query", "unknown"),
        format!("{input};nonce=1"),
        input.replace("nonce", "noncé"),
    ] {
        let mut req = original.clone();
        req.headers.insert(
            "signature-input",
            http::HeaderValue::from_bytes(input.as_bytes()).unwrap(),
        );
        assert!(signature::prepare(&req, NOW).is_err());
    }
    let mut req = signed(Some(vec![]));
    req.headers.remove("content-type");
    assert!(signature::prepare(&req, NOW).is_err());
    let mut req = signed(Some(vec![]));
    req.headers.remove("content-digest");
    assert_eq!(
        signature::prepare(&req, NOW).err().unwrap().code,
        "CONTENT_DIGEST_INVALID"
    );
    // HeaderValue permits a tab, but a Structured Field string does not.
    let mut req = original.clone();
    req.headers.insert(
        "signature-input",
        input.replace("nonce\"", "non\tce\"").parse().unwrap(),
    );
    assert!(signature::prepare(&req, NOW).is_err());
    let store = Arc::new(MemoryReplayStore::new(Arc::new(|| NOW * 1000)));
    let s = store.clone();
    let _ = std::thread::spawn(move || {
        let _lock = s.claims.lock().unwrap();
        panic!("synthetic poisoned lock");
    })
    .join();
    assert!(store.claim("a", "b", NOW + 1).await.unwrap());
}
