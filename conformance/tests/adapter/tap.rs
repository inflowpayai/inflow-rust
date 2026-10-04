use crate::{bad, string};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use inflow_core::Error;
use inflow_tap_seller::{
    ClaimFuture, Clock, Error as TapError, Intent, KeyResolver, KeyResolverOptions,
    MemoryReplayStore, ReplayStore, Request, ResolveFuture, VerifiedFacts, Verifier,
    VerifierOptions, VisaKeyResolver,
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicI64, AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Resolver {
    input: Value,
    time: Arc<AtomicI64>,
}
impl KeyResolver for Resolver {
    fn resolve<'a>(&'a self, keyid: &'a str, _: &'a str) -> ResolveFuture<'a> {
        Box::pin(async move {
            if self.input["resolver_failure"] == true {
                return Err(TapError::new(
                    "CUSTOM_RESOLVER_FAILED",
                    "synthetic resolver failure",
                ));
            }
            if let Some(time) = self.input["resolver_completion_ms"].as_i64() {
                self.time.store(time, Ordering::SeqCst);
            }
            if self.input["key"]["kid"] != keyid {
                return Ok(None);
            }
            let key = URL_SAFE_NO_PAD
                .decode(
                    self.input["key"]["x"]
                        .as_str()
                        .ok_or_else(|| TapError::new("ADAPTER_ERROR", "key"))?,
                )
                .map_err(|_| TapError::new("ADAPTER_ERROR", "key"))?;
            Ok(Some(key.try_into().map_err(|_| {
                TapError::new("ADAPTER_ERROR", "key length")
            })?))
        })
    }
}
struct Store {
    memory: MemoryReplayStore,
    calls: AtomicUsize,
    fail: bool,
}
impl ReplayStore for Store {
    fn claim<'a>(&'a self, keyid: &'a str, nonce: &'a str, expires: i64) -> ClaimFuture<'a> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(TapError::new(
                    "CUSTOM_STORE_FAILED",
                    "synthetic store failure",
                ));
            }
            self.memory.claim(keyid, nonce, expires).await
        })
    }
}
fn facts(f: VerifiedFacts) -> Value {
    json!({"verified":true,"keyid":f.keyid,"algorithm":f.algorithm,"intent":if f.intent==Intent::Browse {"browse"}else{"pay"},"nonce":f.nonce,"created":f.created,"expires":f.expires,"coveredComponents":f.covered_components})
}

pub async fn execute(input: &Value) -> Result<Value, Error> {
    let time = Arc::new(AtomicI64::new(0));
    let t = time.clone();
    let clock: Clock = Arc::new(move || t.load(Ordering::SeqCst));
    let resolver: Arc<dyn KeyResolver> = if input["resolver"] == "http" {
        // Reuse the adapter's loopback validation; TAP itself fetches its configured URL.
        crate::transport::options(input)?;
        Arc::new(
            VisaKeyResolver::new(KeyResolverOptions {
                url: format!("{}/keys", string(input, "base_url")?),
                clock: Some(clock.clone()),
                cache_ttl: Duration::from_millis(input["cache_ttl_ms"].as_u64().unwrap_or(3600000)),
                cache_max_age: Duration::from_millis(
                    input["cache_max_age_ms"].as_u64().unwrap_or(86400000),
                ),
                ..Default::default()
            })
            .map_err(|e| bad(&e.to_string()))?,
        )
    } else {
        Arc::new(Resolver {
            input: input.clone(),
            time: time.clone(),
        })
    };
    let store = Arc::new(Store {
        memory: MemoryReplayStore::new(clock.clone()),
        calls: AtomicUsize::new(0),
        fail: input["store_failure"] == true,
    });
    let verifier = Verifier::new(VerifierOptions {
        key_resolver: Some(resolver),
        replay_store: Some(store.clone()),
        clock: Some(clock),
    })
    .map_err(|e| bad(&e.to_string()))?;
    let handler_calls = AtomicUsize::new(0);
    let mut steps = vec![];
    for step in input["steps"].as_array().ok_or_else(|| bad("steps"))? {
        time.store(
            step["now_ms"].as_i64().ok_or_else(|| bad("time"))?,
            Ordering::SeqCst,
        );
        let mut requests = vec![];
        for item in step["requests"].as_array().ok_or_else(|| bad("requests"))? {
            let mut headers = Vec::new();
            for (name, value) in item["headers"].as_object().ok_or_else(|| bad("headers"))? {
                let values = if let Some(a) = value.as_array() {
                    a.clone()
                } else {
                    vec![value.clone()]
                };
                for v in values {
                    headers.push((name.clone(), string_value(&v)?.to_owned()));
                }
            }
            requests.push(Request::new(
                string(item, "method")?,
                string(item, "url")?,
                headers,
                item.get("body_base64")
                    .map(|v| STANDARD.decode(string_value(v)?).map_err(|_| bad("body")))
                    .transpose()?,
            ));
        }
        let outcomes = futures_util::future::join_all(requests.iter().map(|r| {
            let verifier = &verifier;
            let handler_calls = &handler_calls;
            async move {
                let r = match r {
                    Ok(r) => r,
                    Err(e) => return Err(e.clone()),
                };
                let before = r.clone();
                let result = verifier
                    .with_verified(r, |f| async {
                        handler_calls.fetch_add(1, Ordering::SeqCst);
                        facts(f)
                    })
                    .await;
                assert_eq!(*r, before);
                result
            }
        }))
        .await;
        let mut accepted = vec![];
        let mut rejected = vec![];
        for outcome in outcomes {
            match outcome {
                Ok(f) => accepted.push(f),
                Err(e) => {
                    if ![
                        "SIGNATURE_INPUT_INVALID",
                        "CONTENT_DIGEST_INVALID",
                        "SIGNATURE_LIFETIME_INVALID",
                        "SIGNATURE_NOT_YET_VALID",
                        "SIGNATURE_EXPIRED",
                        "KEY_NOT_FOUND",
                        "KEY_RETRIEVAL_FAILED",
                        "SIGNATURE_INVALID",
                        "NONCE_REPLAYED",
                        "CUSTOM_STORE_FAILED",
                        "CUSTOM_RESOLVER_FAILED",
                    ]
                    .contains(&e.code.as_str())
                    {
                        return Err(bad(&e.to_string()));
                    }
                    rejected.push(e.code);
                }
            }
        }
        rejected.sort();
        steps.push(json!({"accepted":accepted,"rejected":rejected}));
    }
    Ok(
        json!({"steps":steps,"handler_calls":handler_calls.load(Ordering::SeqCst),"claim_calls":store.calls.load(Ordering::SeqCst)}),
    )
}
fn string_value(value: &Value) -> Result<&str, Error> {
    value.as_str().ok_or_else(|| bad("expected string"))
}
