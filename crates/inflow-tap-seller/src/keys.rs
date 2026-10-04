use crate::{Clock, Error, KeyResolver, ResolveFuture, system_clock};
use base64::{
    Engine, alphabet,
    engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig},
};
use futures_util::{
    FutureExt,
    future::{BoxFuture, Shared},
};
use std::{
    collections::{HashMap, HashSet},
    sync::Mutex,
    time::Duration,
};

type Keys = HashMap<String, [u8; 32]>;
type Refresh = Shared<BoxFuture<'static, Result<(Keys, i64), Error>>>;

pub struct KeyResolverOptions {
    pub url: String,
    pub cache_ttl: Duration,
    pub cache_max_age: Duration,
    pub timeout: Duration,
    pub clock: Option<Clock>,
}
impl Default for KeyResolverOptions {
    fn default() -> Self {
        Self {
            url: "https://mcp.visa.com/.well-known/jwks".into(),
            cache_ttl: Duration::from_secs(3600),
            cache_max_age: Duration::from_secs(86400),
            timeout: Duration::from_secs(3),
            clock: None,
        }
    }
}
#[derive(Default)]
struct Cache {
    keys: Keys,
    missing: HashSet<String>,
    updated: Option<i64>,
    refresh: Option<Refresh>,
}

pub struct VisaKeyResolver {
    options: KeyResolverOptions,
    clock: Clock,
    client: reqwest::Client,
    cache: Mutex<Cache>,
}
impl VisaKeyResolver {
    pub fn new(mut options: KeyResolverOptions) -> Result<Self, Error> {
        let url = url::Url::parse(&options.url).map_err(|_| configuration())?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || options.timeout.is_zero()
        {
            return Err(configuration());
        }
        let clock = options.clock.take().unwrap_or_else(system_clock);
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .map_err(|_| configuration())?;
        Ok(Self {
            options,
            clock,
            client,
            cache: Mutex::new(Cache::default()),
        })
    }

    async fn get(&self, keyid: &str) -> Result<Option<[u8; 32]>, Error> {
        let refresh = {
            let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if fresh(cache.updated, (self.clock)(), self.options.cache_ttl)
                && (cache.keys.contains_key(keyid) || cache.missing.contains(keyid))
            {
                return Ok(cache.keys.get(keyid).copied());
            }
            if cache.refresh.is_none() {
                let client = self.client.clone();
                let url = self.options.url.clone();
                let timeout = self.options.timeout;
                let clock = self.clock.clone();
                cache.refresh = Some(
                    async move {
                        let keys = tokio::time::timeout(timeout, load(client, url))
                            .await
                            .map_err(|_| unavailable())??;
                        Ok((keys, clock()))
                    }
                    .boxed()
                    .shared(),
                );
            }
            cache.refresh.as_ref().expect("refresh initialized").clone()
        };
        // Clones share retrieval. Dropping one caller must not discard another's fetch.
        // With no callers polling, the retained future pauses until reuse or resolver drop.
        let result = refresh.clone().await;
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if cache
            .refresh
            .as_ref()
            .is_some_and(|pending| pending.ptr_eq(&refresh))
        {
            cache.refresh = None;
            if let Ok((keys, updated)) = &result {
                cache.keys = keys.clone();
                cache.updated = Some(*updated);
                cache.missing.clear();
            }
        }
        match result {
            Ok(_) => {
                let key = cache.keys.get(keyid).copied();
                if key.is_none() {
                    cache.missing.insert(keyid.to_owned());
                }
                Ok(key)
            }
            Err(error) => {
                if fresh(cache.updated, (self.clock)(), self.options.cache_max_age)
                    && let Some(key) = cache.keys.get(keyid)
                {
                    return Ok(Some(*key));
                }
                Err(error)
            }
        }
    }
}
impl KeyResolver for VisaKeyResolver {
    fn resolve<'a>(&'a self, keyid: &'a str, algorithm: &'a str) -> ResolveFuture<'a> {
        Box::pin(async move {
            if algorithm != "ed25519" {
                return Ok(None);
            }
            self.get(keyid).await
        })
    }
}

fn fresh(updated: Option<i64>, now: i64, age: Duration) -> bool {
    updated.is_some_and(|updated| i128::from(now) - i128::from(updated) <= age.as_millis() as i128)
}
fn unavailable() -> Error {
    Error::new(
        "KEY_RETRIEVAL_FAILED",
        "The TAP verification key could not be retrieved.",
    )
}
fn configuration() -> Error {
    Error::new(
        "INVALID_CONFIGURATION",
        "Invalid TAP key resolver configuration.",
    )
}

async fn load(client: reqwest::Client, url: String) -> Result<Keys, Error> {
    let mut response = client
        .get(url)
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|_| unavailable())?;
    if !response.status().is_success() {
        return Err(unavailable());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| unavailable())? {
        if chunk.len() > 1024 * 1024 - body.len() {
            return Err(unavailable());
        }
        body.extend_from_slice(&chunk);
    }
    parse_keys(&body)
}

fn parse_keys(body: &[u8]) -> Result<Keys, Error> {
    let payload: serde_json::Value = serde_json::from_slice(body).map_err(|_| unavailable())?;
    let object = payload.as_object().ok_or_else(unavailable)?;
    let mut keys = Keys::new();
    if let Some(entries) = object.get("keys") {
        for entry in entries.as_array().ok_or_else(unavailable)? {
            let Some(kid) = entry["kid"].as_str() else {
                continue;
            };
            if !matches!(entry["alg"].as_str(), Some("ed25519" | "Ed25519"))
                || entry["kty"] != "OKP"
                || entry["crv"] != "Ed25519"
                || entry.get("use").is_some_and(|v| v != "sig")
            {
                continue;
            }
            let encoded = entry["x"].as_str().ok_or_else(unavailable)?;
            let decoded = GeneralPurpose::new(
                &alphabet::URL_SAFE,
                GeneralPurposeConfig::new()
                    .with_decode_padding_mode(DecodePaddingMode::Indifferent),
            )
            .decode(encoded)
            .map_err(|_| unavailable())?;
            let bytes: [u8; 32] = decoded.try_into().map_err(|_| unavailable())?;
            ed25519_dalek::VerifyingKey::from_bytes(&bytes).map_err(|_| unavailable())?;
            if keys.insert(kid.to_owned(), bytes).is_some() {
                return Err(unavailable());
            }
        }
    }
    Ok(keys)
}

#[cfg(test)]
#[path = "tests/keys.rs"]
mod tests;
