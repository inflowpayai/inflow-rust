use super::*;
use crate::tests::key;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicI64, AtomicUsize, Ordering},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::Notify,
};

struct Server {
    url: String,
    response: Arc<Mutex<String>>,
    calls: Arc<AtomicUsize>,
    started: Arc<Notify>,
    release: Arc<Notify>,
    blocked: Arc<std::sync::atomic::AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn new() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/keys", listener.local_addr().unwrap());
        let response = Arc::new(Mutex::new(String::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let blocked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (r, c, s, w, b) = (
            response.clone(),
            calls.clone(),
            started.clone(),
            release.clone(),
            blocked.clone(),
        );
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buf = [0; 1024];
                while !request.ends_with(b"\r\n\r\n") {
                    let n = socket.read(&mut buf).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..n]);
                }
                let request = String::from_utf8(request).unwrap();
                assert!(request.starts_with("GET /keys "));
                assert!(request.contains("accept: application/json"));
                assert!(!request.contains("authorization:"));
                c.fetch_add(1, Ordering::SeqCst);
                s.notify_one();
                if b.load(Ordering::SeqCst) {
                    w.notified().await;
                }
                let response = r.lock().unwrap().clone();
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        let server = Self {
            url,
            response,
            calls,
            started,
            release,
            blocked,
            task,
        };
        server.json(json!({"keys":[jwk()]}));
        server
    }
    fn json(&self, value: serde_json::Value) {
        self.raw(200, &value.to_string());
    }
    fn raw(&self, status: u16, body: &str) {
        *self.response.lock().unwrap() = format!(
            "HTTP/1.1 {status} status\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
    }
    fn resolver(&self, clock: Clock) -> VisaKeyResolver {
        VisaKeyResolver::new(KeyResolverOptions {
            url: self.url.clone(),
            clock: Some(clock),
            cache_ttl: Duration::from_secs(10),
            cache_max_age: Duration::from_secs(20),
            ..Default::default()
        })
        .unwrap()
    }
}

#[tokio::test]
async fn http_cache_replacement_and_outage() {
    let server = Server::new().await;
    let time = Arc::new(AtomicI64::new(100));
    let clock = time.clone();
    let resolver = server.resolver(Arc::new(move || clock.load(Ordering::SeqCst) * 1000));
    let expected = Some(key().verifying_key().to_bytes());
    assert_eq!(resolver.resolve("key", "ed25519").await.unwrap(), expected);
    assert_eq!(resolver.resolve("key", "ed25519").await.unwrap(), expected);
    assert_eq!(server.calls.load(Ordering::SeqCst), 1);
    assert_eq!(resolver.resolve("absent", "ed25519").await.unwrap(), None);
    assert_eq!(server.calls.load(Ordering::SeqCst), 2);
    assert_eq!(resolver.resolve("absent", "ed25519").await.unwrap(), None);
    assert_eq!(server.calls.load(Ordering::SeqCst), 2);
    time.store(111, Ordering::SeqCst);
    server.raw(503, "");
    assert_eq!(resolver.resolve("key", "ed25519").await.unwrap(), expected);
    assert_eq!(
        resolver
            .resolve("absent", "ed25519")
            .await
            .unwrap_err()
            .code,
        "KEY_RETRIEVAL_FAILED"
    );
    time.store(121, Ordering::SeqCst);
    assert_eq!(
        resolver.resolve("key", "ed25519").await.unwrap_err().code,
        "KEY_RETRIEVAL_FAILED"
    );
    server.json(json!({"keys":[]}));
    assert_eq!(resolver.resolve("key", "ed25519").await.unwrap(), None);
    server.raw(503, "");
    time.store(132, Ordering::SeqCst);
    assert!(resolver.resolve("key", "ed25519").await.is_err());
}

#[tokio::test]
async fn concurrent_refresh_and_cancelled_waiter() {
    let server = Server::new().await;
    server.blocked.store(true, Ordering::SeqCst);
    let resolver = Arc::new(server.resolver(Arc::new(|| 100)));
    let r = resolver.clone();
    let first = tokio::spawn(async move { r.resolve("key", "ed25519").await });
    server.started.notified().await;
    let r = resolver.clone();
    let second = tokio::spawn(async move { r.resolve("key", "ed25519").await });
    tokio::task::yield_now().await;
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    server.release.notify_one();
    assert!(second.await.unwrap().unwrap().is_some());
    assert_eq!(server.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn http_failures_are_bounded_and_not_followed() {
    let server = Server::new().await;
    for (status, body) in [
        (302, "".to_owned()),
        (200, "not json".into()),
        (200, "x".repeat(1024 * 1024 + 1)),
    ] {
        server.raw(status, &body);
        let resolver = server.resolver(Arc::new(|| 100));
        assert!(resolver.resolve("key", "ed25519").await.is_err());
    }
    *server.response.lock().unwrap() =
        "HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n{".into();
    assert!(
        server
            .resolver(Arc::new(|| 100))
            .resolve("key", "ed25519")
            .await
            .is_err()
    );
    server.blocked.store(true, Ordering::SeqCst);
    let resolver = VisaKeyResolver::new(KeyResolverOptions {
        url: server.url.clone(),
        timeout: Duration::from_millis(10),
        ..Default::default()
    })
    .unwrap();
    assert!(resolver.resolve("key", "ed25519").await.is_err());
    let url = server.url.clone();
    drop(server);
    assert!(
        VisaKeyResolver::new(KeyResolverOptions {
            url,
            ..Default::default()
        })
        .unwrap()
        .resolve("key", "ed25519")
        .await
        .is_err()
    );
}

#[tokio::test]
async fn concurrent_waiters_share_success_and_failure() {
    for status in [200, 503] {
        let server = Server::new().await;
        if status == 503 {
            server.raw(status, "");
        }
        server.blocked.store(true, Ordering::SeqCst);
        let resolver = server.resolver(Arc::new(|| 100));
        let first = resolver.resolve("key", "ed25519");
        let second = resolver.resolve("key", "ed25519");
        let release = async {
            server.started.notified().await;
            server.release.notify_one();
        };
        let (first, second, ()) = tokio::join!(first, second, release);
        assert_eq!(first, second);
        assert_eq!(first.is_ok(), status == 200);
        assert_eq!(server.calls.load(Ordering::SeqCst), 1);
    }
}

fn jwk() -> serde_json::Value {
    json!({"kid":"key","alg":"Ed25519","kty":"OKP","crv":"Ed25519","use":"sig","x":base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key().verifying_key().to_bytes())})
}
#[test]
fn parses_only_trusted_eligible_keys() {
    assert_eq!(
        parse_keys(&serde_json::to_vec(&json!({"keys":[jwk()]})).unwrap())
            .unwrap()
            .len(),
        1
    );
    for payload in [
        json!({}),
        json!({"keys":[]}),
        json!({"keys":[null,{}, {"kid":"x","alg":"rsa"}]}),
    ] {
        assert!(
            parse_keys(&serde_json::to_vec(&payload).unwrap())
                .unwrap()
                .is_empty()
        );
    }
    for field in ["kid", "alg", "kty", "crv", "use"] {
        let mut key = jwk();
        key[field] = json!(false);
        assert!(
            parse_keys(&serde_json::to_vec(&json!({"keys":[key]})).unwrap())
                .unwrap()
                .is_empty()
        );
    }
    for payload in [
        json!(null),
        json!([]),
        json!({"keys":null}),
        json!({"keys":[jwk(),jwk()]}),
    ] {
        assert!(parse_keys(&serde_json::to_vec(&payload).unwrap()).is_err());
    }
    assert!(parse_keys(b"invalid").is_err());
    for x in [
        json!(null),
        json!("!"),
        json!("AA"),
        json!(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([2; 32])),
    ] {
        let mut key = jwk();
        key["x"] = x;
        assert!(parse_keys(&serde_json::to_vec(&json!({"keys":[key]})).unwrap()).is_err());
    }
}
#[tokio::test]
async fn options_and_algorithm() {
    for url in [
        "invalid",
        "ftp://example/a",
        "https://a:b@example/a",
        "https://example/a#fragment",
    ] {
        assert!(
            VisaKeyResolver::new(KeyResolverOptions {
                url: url.into(),
                ..Default::default()
            })
            .is_err()
        );
    }
    assert!(
        VisaKeyResolver::new(KeyResolverOptions {
            timeout: Duration::ZERO,
            ..Default::default()
        })
        .is_err()
    );
    let resolver = VisaKeyResolver::new(Default::default()).unwrap();
    assert_eq!(resolver.resolve("key", "rsa").await.unwrap(), None);
    assert!(!fresh(None, 1, Duration::ZERO));
    assert!(fresh(Some(1), 1, Duration::ZERO));
}

#[tokio::test]
async fn subsecond_cache_expiry_and_cancelled_refresh_resume() {
    let server = Server::new().await;
    let time = Arc::new(AtomicI64::new(100_000));
    let clock = time.clone();
    let resolver = Arc::new(
        VisaKeyResolver::new(KeyResolverOptions {
            url: server.url.clone(),
            clock: Some(Arc::new(move || clock.load(Ordering::SeqCst))),
            cache_ttl: Duration::from_millis(10),
            ..Default::default()
        })
        .unwrap(),
    );
    assert!(resolver.resolve("key", "ed25519").await.unwrap().is_some());
    time.store(100_011, Ordering::SeqCst);
    server.blocked.store(true, Ordering::SeqCst);
    // Consume the notification from the first successful fetch.
    server.started.notified().await;
    let r = resolver.clone();
    let waiter = tokio::spawn(async move { r.resolve("key", "ed25519").await });
    server.started.notified().await;
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    server.release.notify_one();
    assert!(resolver.resolve("key", "ed25519").await.unwrap().is_some());
    assert_eq!(server.calls.load(Ordering::SeqCst), 2);
    let r = resolver.clone();
    let _ = std::thread::spawn(move || {
        let _lock = r.cache.lock().unwrap();
        panic!("synthetic poisoned lock");
    })
    .join();
    assert!(resolver.resolve("key", "ed25519").await.unwrap().is_some());
    server.blocked.store(false, Ordering::SeqCst);
    time.store(100_022, Ordering::SeqCst);
    assert!(resolver.resolve("key", "ed25519").await.unwrap().is_some());
}
