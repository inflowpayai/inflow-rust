use crate::{CancellationToken, ClientOptions, Error, OriginalJson, PaymentRequired, invalid};
use inflow_x402::{PaymentRequirements, internal::X402Client};
use serde_json::Value;
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{sync::Mutex, time::Instant};

#[derive(Clone)]
pub struct BuyerOptions {
    pub client: ClientOptions,
    pub prefer: Vec<String>,
}

impl Default for BuyerOptions {
    fn default() -> Self {
        Self {
            client: ClientOptions::default(),
            prefer: vec!["balance".into(), "exact".into()],
        }
    }
}

#[derive(Clone)]
pub struct Buyer {
    pub(crate) client: X402Client,
    prefer: Vec<String>,
    supported: Arc<Mutex<Supported>>,
    generation: Arc<AtomicU64>,
}

struct Supported {
    value: Value,
    fetched: Instant,
}

impl Buyer {
    /// Fetches Buyer capabilities before returning. Does not create an approval.
    pub async fn new(
        options: BuyerOptions,
        cancellation: &CancellationToken,
    ) -> Result<Self, Error> {
        let client = X402Client::new(options.client)?;
        let value = read_supported(&client, cancellation).await?;
        Ok(Self {
            client,
            prefer: options.prefer,
            supported: Arc::new(Mutex::new(Supported {
                value,
                fetched: Instant::now(),
            })),
            generation: Arc::new(AtomicU64::new(0)),
        })
    }

    pub async fn supported(&self, cancellation: &CancellationToken) -> Result<Value, Error> {
        self.load_supported(false, cancellation).await
    }

    pub async fn refresh_supported(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<Value, Error> {
        self.load_supported(true, cancellation).await
    }

    async fn load_supported(
        &self,
        force: bool,
        cancellation: &CancellationToken,
    ) -> Result<Value, Error> {
        let observed = self.generation.load(Ordering::Acquire);
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(cancelled()),
            result = async {
                let mut cache = self.supported.lock().await;
                // A concurrent refresh satisfies this call too. Failed reads leave the snapshot intact.
                if cache.fetched.elapsed() >= Duration::from_secs(3600)
                    || (force && self.generation.load(Ordering::Acquire) == observed)
                {
                    let value = read_supported(&self.client, cancellation).await?;
                    *cache = Supported { value, fetched: Instant::now() };
                    self.generation.fetch_add(1, Ordering::Release);
                }
                Ok(cache.value.clone())
            } => result,
        }
    }

    pub async fn supports(
        &self,
        requirement: &OriginalJson,
        cancellation: &CancellationToken,
    ) -> Result<bool, Error> {
        Ok(supports(
            &self.supported(cancellation).await?,
            &read(requirement)?,
        ))
    }

    /// Keeps scheme preference first; fresh ledger balances break ties within the balance scheme.
    pub async fn select(
        &self,
        required: &PaymentRequired<OriginalJson>,
        cancellation: &CancellationToken,
    ) -> Result<Option<OriginalJson>, Error> {
        let supported = self.supported(cancellation).await?;
        let candidates: Vec<_> = required
            .accepts
            .iter()
            .map(|original| read(original).map(|value| (original, value)))
            .collect::<Result<_, _>>()?;
        for scheme in &self.prefer {
            let matches: Vec<_> = candidates
                .iter()
                .filter(|(_, value)| value["scheme"] == *scheme && supports(&supported, value))
                .collect();
            if let Some(first) = matches.first() {
                if scheme == "balance" && matches.len() > 1 {
                    // Balance lookup is advisory, not authorization. Cancellation must not become fallback signing.
                    let balances = self.client.balances(cancellation).await;
                    if cancellation.is_cancelled() {
                        return Err(cancelled());
                    }
                    if let Ok(balances) = balances {
                        for candidate in &matches {
                            if covers(&balances, &candidate.1) {
                                return Ok(Some(candidate.0.clone()));
                            }
                        }
                    }
                }
                return Ok(Some(first.0.clone()));
            }
        }
        Ok(None)
    }

    pub async fn payload(
        &self,
        transaction_id: &str,
        cancellation: &CancellationToken,
    ) -> Result<Value, Error> {
        self.client.poll(transaction_id, cancellation).await
    }

    pub async fn cancel_approval(&self, approval_id: &str) -> Result<(), Error> {
        self.client.approval_cleanup(approval_id)?.cancel().await
    }
}

async fn read_supported(client: &X402Client, token: &CancellationToken) -> Result<Value, Error> {
    let value = client.buyer_supported(token).await?;
    if !value["kinds"].is_array() {
        return Err(invalid("Buyer capabilities have no kinds array"));
    }
    Ok(value)
}

fn supports(supported: &Value, requirement: &Value) -> bool {
    requirement["extra"]["assetTransferMethod"] != "permit2"
        && supported["kinds"].as_array().is_some_and(|kinds| {
            kinds.iter().any(|kind| {
                kind["scheme"] == requirement["scheme"] && kind["network"] == requirement["network"]
            })
        })
}

pub(crate) fn read(requirement: &OriginalJson) -> Result<Value, Error> {
    let _: PaymentRequirements = PaymentRequirements::try_from(requirement)
        .map_err(|_| invalid("invalid payment requirements"))?;
    serde_json::from_str(requirement.0.get()).map_err(|_| invalid("invalid payment requirements"))
}

fn covers(balances: &Value, requirement: &Value) -> bool {
    let Some(asset) = requirement["extra"]["assetName"].as_str() else {
        return false;
    };
    let Some(required) = requirement["amount"].as_str().and_then(integer) else {
        return false;
    };
    balances["balances"].as_array().is_some_and(|list| {
        list.iter()
            .rev()
            .filter(|balance| balance["currency"] == asset)
            .find_map(|balance| balance["available"].as_str().and_then(atomic))
            .is_some_and(|available| {
                !available.starts_with('-')
                    && (available.len() > required.len()
                        || (available.len() == required.len() && available.as_str() >= required))
            })
    })
}

fn integer(value: &str) -> Option<&str> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let value = value.trim_start_matches('0');
    Some(if value.is_empty() { "0" } else { value })
}

fn atomic(value: &str) -> Option<String> {
    let value = value.trim();
    let (negative, value) = value
        .strip_prefix('-')
        .map_or((false, value), |v| (true, v));
    let (whole, fraction) = match value.split_once('.') {
        Some((_, "")) => return None,
        Some(parts) => parts,
        None => (value, ""),
    };
    integer(whole)?;
    if !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let padded = format!("{whole}{fraction:0<18}");
    integer(&padded[..whole.len() + 18]).map(|v| {
        if negative && v != "0" {
            format!("-{v}")
        } else {
            v.to_owned()
        }
    })
}

pub(crate) fn cancelled() -> Error {
    Error::new(
        "X402_APPROVAL_CANCELLED",
        "x402 payment cancelled by caller",
    )
}
