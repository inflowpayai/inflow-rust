use crate::{CancellationToken, Error, PaymentRequirements, Seller, invalid};
use serde::Deserialize;
use serde_json::{Map, Value, json};

const PROXY: &str = "0x402085c248EeA27D92E8b30b2C58ed07f9E20001";

#[derive(Clone, Debug)]
pub struct Price {
    pub amount: String,
    pub currency: Option<String>,
}

impl From<&str> for Price {
    fn from(amount: &str) -> Self {
        Self {
            amount: amount.into(),
            currency: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct OfferOptions {
    pub price: Price,
    pub max_timeout_seconds: u64,
    pub schemes: Option<Vec<String>>,
    pub networks: Option<Vec<String>>,
    /// Uses Permit2 for compatible on-chain assets; balance offers are unaffected.
    pub permit2: bool,
}

impl OfferOptions {
    pub fn new(price: impl Into<Price>) -> Self {
        Self {
            price: price.into(),
            max_timeout_seconds: 300,
            schemes: None,
            networks: None,
            permit2: false,
        }
    }

    fn includes(&self, scheme: &str, network: &str) -> bool {
        self.schemes
            .as_ref()
            .is_none_or(|list| list.iter().any(|v| v == scheme))
            && self
                .networks
                .as_ref()
                .is_none_or(|list| list.iter().any(|v| v == network))
    }
}

#[derive(Clone, Debug)]
pub struct Route {
    pub accepts: Vec<PaymentRequirements>,
    pub extensions: Map<String, Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    assets: Vec<Asset>,
    wallets: Vec<Wallet>,
    payment_methods: Vec<Method>,
    #[serde(default)]
    supported: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Asset {
    blockchain: String,
    currency: String,
    network: String,
    asset_id: String,
    asset_name: String,
    decimals: u8,
    token_name: Option<String>,
    token_version: Option<String>,
    asset_transfer_method: Option<String>,
    permit2_proxy: Option<String>,
    #[serde(default)]
    supports_eip2612: bool,
    #[serde(default)]
    supports_eip7702: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Wallet {
    blockchain: String,
    address: String,
    fee_payer: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Method {
    scheme: String,
    network: String,
    pay_to: String,
    decimals: u8,
    extra: Option<Map<String, Value>>,
}

impl Seller {
    pub async fn offers(
        &self,
        options: &OfferOptions,
        token: &CancellationToken,
    ) -> Result<Vec<PaymentRequirements>, Error> {
        build(self.config(token).await?, options)
    }

    pub async fn route(
        &self,
        options: &OfferOptions,
        token: &CancellationToken,
    ) -> Result<Route, Error> {
        let accepts = self.offers(options, token).await?;
        let permit2: Vec<_> = accepts
            .iter()
            .filter(|v| extra(v)["assetTransferMethod"] == "permit2")
            .collect();
        let mut extensions = Map::new();
        if !permit2.is_empty()
            && permit2.iter().all(|offer| {
                offer.scheme == "exact"
                    && offer.network.to_string().starts_with("eip155:")
                    && extra(offer)["permit2Proxy"]
                        .as_str()
                        .is_some_and(|s| s.eq_ignore_ascii_case(PROXY))
            })
        {
            let eip2612 = permit2.iter().all(|v| {
                extra(v)["supportsEip2612"] == true
                    && ["name", "version"]
                        .iter()
                        .all(|key| extra(v)[key].as_str().is_some_and(|v| !v.is_empty()))
            });
            let eip7702 = permit2.iter().all(|v| extra(v)["supportsEip7702"] == true);
            if eip2612 || eip7702 {
                let supported = self.refresh_supported(token).await?;
                for (key, enabled) in [
                    ("eip2612GasSponsoring", eip2612),
                    ("inflowEip7702GasSponsoring", eip7702),
                ] {
                    if enabled
                        && supported.extensions.iter().any(|v| v == key)
                        && permit2.iter().all(|offer| {
                            supported.kinds.iter().any(|kind| {
                                kind.x402_version == 2
                                    && kind.scheme == offer.scheme
                                    && kind.network == offer.network.to_string()
                                    && (key != "inflowEip7702GasSponsoring"
                                        || kind
                                            .extra
                                            .as_ref()
                                            .is_some_and(|v| v["supportsEip7702"] == true))
                            })
                        })
                    {
                        let declaration = if key == "eip2612GasSponsoring" {
                            serde_json::from_str(include_str!("eip2612.json"))
                                .map_err(|_| invalid("invalid EIP-2612 declaration"))?
                        } else {
                            json!({"info":{"version":"1"}})
                        };
                        extensions.insert(key.into(), declaration);
                        break;
                    }
                }
            }
        }
        Ok(Route {
            accepts,
            extensions,
        })
    }
}

fn extra(offer: &PaymentRequirements) -> &Value {
    offer.extra.as_ref().unwrap_or(&Value::Null)
}

fn build(value: Value, options: &OfferOptions) -> Result<Vec<PaymentRequirements>, Error> {
    let config: Config =
        serde_json::from_value(value).map_err(|_| invalid("invalid Seller configuration"))?;
    let (whole, fraction, currency) = price(&options.price)?;
    let mut result = Vec::new();
    for wallet in &config.wallets {
        for asset in config.assets.iter().filter(|asset| {
            asset.blockchain == wallet.blockchain
                && (currency == "USD" || currency == asset.currency)
        }) {
            let method = if options.permit2 {
                Some("permit2")
            } else {
                asset.asset_transfer_method.as_deref()
            };
            let compatible = !options.permit2
                || (asset.network.starts_with("eip155:")
                    && asset
                        .permit2_proxy
                        .as_deref()
                        .is_some_and(|p| p.eq_ignore_ascii_case(PROXY)));
            if compatible && options.includes("exact", &asset.network) {
                result.push(onchain(
                    asset,
                    wallet,
                    method,
                    "exact",
                    None,
                    atomic(whole, fraction, asset.decimals)?,
                    options.max_timeout_seconds,
                )?);
            }
            if options
                .schemes
                .as_ref()
                .is_some_and(|s| s.iter().any(|s| s == "upto"))
                && options.includes("upto", &asset.network)
                && asset.network.starts_with("eip155:")
                && asset.permit2_proxy.as_ref().is_some_and(|v| !v.is_empty())
            {
                let kind = config.supported.iter().find(|k| {
                    k["x402Version"] == 2
                        && k["scheme"] == "upto"
                        && k["network"] == asset.network
                        && k["extra"]["assetTransferMethod"] == "permit2"
                        && ["permit2Proxy", "facilitatorAddress"]
                            .iter()
                            .all(|key| k["extra"][key].as_str().is_some_and(|s| !s.is_empty()))
                });
                if let Some(kind) = kind {
                    result.push(onchain(
                        asset,
                        wallet,
                        Some("permit2"),
                        "upto",
                        kind["extra"].as_object(),
                        atomic(whole, fraction, asset.decimals)?,
                        options.max_timeout_seconds,
                    )?);
                }
            }
        }
    }
    let mut currencies = Vec::new();
    for asset in &config.assets {
        if !currencies.contains(&asset.currency.as_str()) {
            currencies.push(asset.currency.as_str());
        }
    }
    if currency != "USD" {
        currencies = vec![currency];
    }
    for method in &config.payment_methods {
        if !options.includes(&method.scheme, &method.network) {
            continue;
        }
        let instrument = method.scheme == "instrument";
        if instrument {
            if currency != "USD"
                || !options
                    .schemes
                    .as_ref()
                    .is_some_and(|s| s.iter().any(|s| s == "instrument"))
            {
                continue;
            }
            let cents = atomic(whole, fraction, 2)?;
            if !cents.parse::<i64>().is_ok_and(|cents| cents >= 50) {
                return Err(invalid(
                    "instrument payments require USD 0.50–92233720368547758.07 in whole cents",
                ));
            }
        }
        // Instrument USD is fiat, independent of configured blockchain assets.
        for currency in if instrument {
            &["USD"][..]
        } else {
            &currencies
        } {
            let mut extra = method.extra.clone().unwrap_or_default();
            extra.insert("assetName".into(), json!(currency));
            result.push(requirement(
                &method.scheme,
                &method.network,
                currency,
                &method.pay_to,
                atomic(whole, fraction, method.decimals)?,
                options.max_timeout_seconds,
                extra,
            )?);
        }
    }
    Ok(result)
}

fn onchain(
    asset: &Asset,
    wallet: &Wallet,
    method: Option<&str>,
    scheme: &str,
    scheme_extra: Option<&Map<String, Value>>,
    amount: String,
    timeout: u64,
) -> Result<PaymentRequirements, Error> {
    let mut extra = Map::new();
    extra.insert("assetName".into(), json!(asset.asset_name));
    for (key, value) in [
        ("name", asset.token_name.as_deref()),
        ("version", asset.token_version.as_deref()),
        ("assetTransferMethod", method),
        ("feePayer", wallet.fee_payer.as_deref()),
    ] {
        if let Some(value) = value {
            extra.insert(key.into(), json!(value));
        }
    }
    if method == Some("permit2") {
        if let Some(proxy) = &asset.permit2_proxy {
            extra.insert("permit2Proxy".into(), json!(proxy));
        }
        for (key, enabled) in [
            ("supportsEip2612", asset.supports_eip2612),
            ("supportsEip7702", asset.supports_eip7702),
        ] {
            if enabled {
                extra.insert(key.into(), json!(true));
            }
        }
    }
    if let Some(values) = scheme_extra {
        extra.extend(values.clone());
    }
    requirement(
        scheme,
        &asset.network,
        &asset.asset_id,
        &wallet.address,
        amount,
        timeout,
        extra,
    )
}

fn requirement(
    scheme: &str,
    network: &str,
    asset: &str,
    pay_to: &str,
    amount: String,
    max_timeout_seconds: u64,
    extra: Map<String, Value>,
) -> Result<PaymentRequirements, Error> {
    Ok(PaymentRequirements {
        scheme: scheme.into(),
        network: network
            .parse()
            .map_err(|_| invalid("invalid payment network"))?,
        amount,
        pay_to: pay_to.into(),
        max_timeout_seconds,
        asset: asset.into(),
        extra: Some(Value::Object(extra)),
    })
}

fn price(price: &Price) -> Result<(&str, &str, &str), Error> {
    if price.amount.trim() != price.amount {
        return Err(invalid(
            "price must not contain leading or trailing whitespace",
        ));
    }
    let words: Vec<_> = price.amount.split_whitespace().collect();
    let (amount, embedded) = match words.as_slice() {
        [amount] if amount.starts_with('$') => (&amount[1..], Some("USD")),
        [amount] => (*amount, None),
        [amount, currency]
            if currency
                .bytes()
                .next()
                .is_some_and(|c| c.is_ascii_uppercase())
                && currency
                    .bytes()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_') =>
        {
            (*amount, Some(*currency))
        }
        _ => {
            return Err(invalid(
                "invalid price; use $0.01, 0.01 USDC, or an explicit currency",
            ));
        }
    };
    let (whole, fraction) = amount.split_once('.').unwrap_or((amount, ""));
    if whole.is_empty()
        || !whole.bytes().all(|c| c.is_ascii_digit())
        || !fraction.bytes().all(|c| c.is_ascii_digit())
        || fraction.len() > 8
        || (amount.contains('.') && fraction.is_empty())
    {
        return Err(invalid(
            "price must be a nonnegative decimal with at most eight decimal places",
        ));
    }
    Ok((
        whole,
        fraction,
        price
            .currency
            .as_deref()
            .or(embedded)
            .ok_or_else(|| invalid("price requires a currency"))?,
    ))
}

fn atomic(whole: &str, fraction: &str, decimals: u8) -> Result<String, Error> {
    let places = usize::from(decimals);
    if fraction.len() > places && fraction[places..].bytes().any(|c| c != b'0') {
        return Err(invalid("price cannot be represented without truncation"));
    }
    let fraction = &fraction[..fraction.len().min(places)];
    let amount = format!("{whole}{fraction:0<places$}");
    let amount = amount.trim_start_matches('0');
    Ok(if amount.is_empty() {
        "0".into()
    } else {
        amount.into()
    })
}
