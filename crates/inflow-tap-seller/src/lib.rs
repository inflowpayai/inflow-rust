#![doc = include_str!("../README.md")]

mod keys;
mod signature;

pub use http::HeaderMap;
pub use keys::{KeyResolverOptions, VisaKeyResolver};
use std::{
    collections::HashMap,
    fmt,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

/// Unix milliseconds, matching Node. Signature timestamps remain integer seconds.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;
pub type ResolveFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<[u8; 32]>, Error>> + Send + 'a>>;
pub type ClaimFuture<'a> = Pin<Box<dyn Future<Output = Result<bool, Error>> + Send + 'a>>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Error {
    pub code: String,
    pub message: String,
}
impl Error {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Error {}

pub trait KeyResolver: Send + Sync {
    /// Return trusted Ed25519 public-key bytes. Untrusted input must not select a fetch URL.
    fn resolve<'a>(&'a self, keyid: &'a str, algorithm: &'a str) -> ResolveFuture<'a>;
}
pub trait ReplayStore: Send + Sync {
    /// Atomically claim this pair until expiration; return false if already retained.
    fn claim<'a>(&'a self, keyid: &'a str, nonce: &'a str, expires: i64) -> ClaimFuture<'a>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Request {
    pub method: String,
    pub url: String,
    pub headers: HeaderMap,
    /// None is no body; Some(vec![]) is an explicitly supplied empty body.
    pub body: Option<Vec<u8>>,
}
impl Request {
    /// Convert raw header pairs without collapsing duplicates. Malformed HTTP fields fail here.
    pub fn new(
        method: impl Into<String>,
        url: impl Into<String>,
        headers: impl IntoIterator<Item = (String, String)>,
        body: Option<Vec<u8>>,
    ) -> Result<Self, Error> {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            map.append(
                http::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| invalid())?,
                http::HeaderValue::from_str(&value).map_err(|_| invalid())?,
            );
        }
        Ok(Self {
            method: method.into(),
            url: url.into(),
            headers: map,
            body,
        })
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Intent {
    Browse,
    Pay,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedFacts {
    pub keyid: String,
    pub algorithm: &'static str,
    pub intent: Intent,
    pub nonce: String,
    pub created: i64,
    pub expires: i64,
    pub covered_components: Vec<String>,
}

pub struct MemoryReplayStore {
    claims: Mutex<HashMap<(String, String), i64>>,
    clock: Clock,
}
impl MemoryReplayStore {
    pub fn new(clock: Clock) -> Self {
        Self {
            claims: Mutex::new(HashMap::new()),
            clock,
        }
    }
}
impl Default for MemoryReplayStore {
    fn default() -> Self {
        Self::new(system_clock())
    }
}
impl ReplayStore for MemoryReplayStore {
    fn claim<'a>(&'a self, keyid: &'a str, nonce: &'a str, expires: i64) -> ClaimFuture<'a> {
        Box::pin(async move {
            let now = (self.clock)().div_euclid(1000);
            let mut claims = self.claims.lock().unwrap_or_else(|e| e.into_inner());
            claims.retain(|_, expires| *expires > now);
            let key = (keyid.to_owned(), nonce.to_owned());
            if claims.contains_key(&key) {
                return Ok(false);
            }
            claims.insert(key, expires);
            Ok(true)
        })
    }
}

#[derive(Default)]
pub struct VerifierOptions {
    pub key_resolver: Option<Arc<dyn KeyResolver>>,
    pub replay_store: Option<Arc<dyn ReplayStore>>,
    pub clock: Option<Clock>,
}

#[derive(Clone)]
pub struct Verifier {
    resolver: Arc<dyn KeyResolver>,
    replay: Arc<dyn ReplayStore>,
    clock: Clock,
}
impl Verifier {
    pub fn new(options: VerifierOptions) -> Result<Self, Error> {
        let clock = options.clock.unwrap_or_else(system_clock);
        let resolver = match options.key_resolver {
            Some(resolver) => resolver,
            None => Arc::new(VisaKeyResolver::new(KeyResolverOptions {
                clock: Some(clock.clone()),
                ..Default::default()
            })?),
        };
        Ok(Self {
            resolver,
            replay: options
                .replay_store
                .unwrap_or_else(|| Arc::new(MemoryReplayStore::new(clock.clone()))),
            clock,
        })
    }

    pub async fn verify(&self, request: &Request) -> Result<VerifiedFacts, Error> {
        let parsed = signature::prepare(request, (self.clock)().div_euclid(1000))?;
        let key = self
            .resolver
            .resolve(&parsed.facts.keyid, "ed25519")
            .await?
            .ok_or_else(|| {
                Error::new("KEY_NOT_FOUND", "The TAP verification key was not found.")
            })?;
        let invalid = || Error::new("SIGNATURE_INVALID", "The TAP signature is invalid.");
        let key = ed25519_dalek::VerifyingKey::from_bytes(&key).map_err(|_| invalid())?;
        let signature =
            ed25519_dalek::Signature::from_slice(&parsed.signature).map_err(|_| invalid())?;
        key.verify_strict(parsed.base.as_bytes(), &signature)
            .map_err(|_| invalid())?;
        if !self
            .replay
            .claim(
                &parsed.facts.keyid,
                &parsed.facts.nonce,
                parsed.facts.expires,
            )
            .await?
        {
            return Err(Error::new(
                "NONCE_REPLAYED",
                "The TAP nonce has already been used.",
            ));
        }
        Ok(parsed.facts)
    }

    /// The handler runs only after signature verification and a successful replay claim.
    pub async fn with_verified<T, F, Fut>(&self, request: &Request, handler: F) -> Result<T, Error>
    where
        F: FnOnce(VerifiedFacts) -> Fut,
        Fut: Future<Output = T>,
    {
        Ok(handler(self.verify(request).await?).await)
    }
}

fn system_clock() -> Clock {
    Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|v| v.as_millis() as i64)
            .unwrap_or_default()
    })
}
fn invalid() -> Error {
    Error::new(
        "SIGNATURE_INPUT_INVALID",
        "The TAP signed request is invalid.",
    )
}

#[cfg(test)]
#[path = "tests/verifier.rs"]
mod tests;
