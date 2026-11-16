use crate::{Error, PaymentChallenge};
use serde_json::Value;

/// Optional per-payment restrictions. None leaves a field unrestricted;
/// an empty allowlist permits no values. Amounts are exact decimal strings.
#[derive(Clone, Default)]
pub struct MethodLimits {
    pub max_amount: Option<String>,
    pub max_atomic_amount: Option<String>,
    pub max_amount_decimals: Option<u8>,
    pub currencies: Option<Vec<String>>,
    pub expected_recipients: Option<Vec<String>>,
    pub allowed_chain_ids: Option<Vec<u64>>,
}

fn invalid(message: &str) -> Error {
    Error::new("MPP_PAYMENT_LIMIT", message)
}

fn decimal(value: &str) -> Result<(&str, &str), Error> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
        || (value.contains('.') && fraction.is_empty())
    {
        return Err(invalid("amount must be a nonnegative decimal string"));
    }
    Ok((whole, fraction))
}

fn atomic(value: &str, precision: usize) -> Result<String, Error> {
    let (whole, fraction) = decimal(value)?;
    let fraction = fraction.trim_end_matches('0');
    if fraction.len() > precision {
        return Err(invalid("amount exceeds currency precision"));
    }
    Ok(format!(
        "{whole}{fraction}{}",
        "0".repeat(precision - fraction.len())
    ))
}

fn exceeds(value: &str, maximum: &str) -> bool {
    let value = value.trim_start_matches('0');
    let maximum = maximum.trim_start_matches('0');
    value.len() > maximum.len() || (value.len() == maximum.len() && value > maximum)
}

impl MethodLimits {
    pub(crate) fn validate(&self, method: &str) -> Result<(), Error> {
        if method != "tempo"
            && (self.max_amount_decimals.is_some() || self.allowed_chain_ids.is_some())
        {
            return Err(invalid(
                "decimals and chain restrictions apply only to Tempo",
            ));
        }
        if method == "inflow" && self.max_atomic_amount.is_some() {
            return Err(invalid("InFlow amounts use currency units; use max_amount"));
        }
        if method == "card" && (self.currencies.is_some() || self.expected_recipients.is_some()) {
            return Err(invalid("CARD supports amount limits only"));
        }
        for value in self
            .currencies
            .iter()
            .chain(self.expected_recipients.iter())
            .flatten()
        {
            if value.is_empty() {
                return Err(invalid("allowlists require nonempty strings"));
            }
        }
        if self
            .allowed_chain_ids
            .as_ref()
            .is_some_and(|ids| ids.iter().any(|id| *id > 9_007_199_254_740_991))
        {
            return Err(invalid("chain IDs must be safe nonnegative integers"));
        }
        if method != "card"
            && (self.max_amount.is_some() || self.max_atomic_amount.is_some())
            && self.currencies.is_none()
        {
            return Err(invalid("amount limits require currencies"));
        }
        if let Some(maximum) = &self.max_amount {
            decimal(maximum)?;
            if method == "tempo" && self.max_amount_decimals.is_none() {
                return Err(invalid("Tempo max_amount requires max_amount_decimals"));
            }
            if method != "inflow" {
                atomic(maximum, usize::from(self.max_amount_decimals.unwrap_or(2)))?;
            }
        }
        if let Some(maximum) = &self.max_atomic_amount
            && (maximum.is_empty() || !maximum.bytes().all(|b| b.is_ascii_digit()))
        {
            return Err(invalid("max_atomic_amount requires a nonnegative integer"));
        }
        Ok(())
    }

    pub(crate) fn check(&self, challenge: &PaymentChallenge) -> Result<(), Error> {
        let request: Value = challenge
            .request
            .decode()
            .map_err(|_| invalid("invalid challenge request"))?;
        inflow_mpp::validate_request(
            challenge.method.as_str(),
            challenge.intent.as_str(),
            &request,
        )?;
        let tempo = challenge.method.as_str() == "tempo";
        let allowed = |list: &Option<Vec<String>>, value: &Value| {
            list.as_ref().is_none_or(|values| {
                value.as_str().is_some_and(|value| {
                    values.iter().any(|expected| {
                        if tempo {
                            expected.eq_ignore_ascii_case(value)
                        } else {
                            expected == value
                        }
                    })
                })
            })
        };
        if !allowed(&self.currencies, &request["currency"]) {
            return Err(invalid("charge currency is not allowed"));
        }
        if !allowed(&self.expected_recipients, &request["recipient"]) {
            return Err(invalid("charge recipient is not allowed"));
        }
        if tempo {
            if let Some(splits) = request["methodDetails"]["splits"].as_array() {
                for split in splits {
                    if !allowed(&self.expected_recipients, &split["recipient"]) {
                        return Err(invalid("split recipient is not allowed"));
                    }
                }
            }
            if self.allowed_chain_ids.as_ref().is_some_and(|ids| {
                request["methodDetails"]["chainId"]
                    .as_u64()
                    .is_none_or(|id| !ids.contains(&id))
            }) {
                return Err(invalid("charge chain is not allowed"));
            }
        }
        let amount = request["amount"].as_str().unwrap_or_default();
        if let Some(maximum) = &self.max_amount {
            let precision = if challenge.method.as_str() == "inflow" {
                decimal(amount)?.1.len().max(decimal(maximum)?.1.len())
            } else {
                usize::from(self.max_amount_decimals.unwrap_or(2))
            };
            let value = if challenge.method.as_str() == "inflow" {
                atomic(amount, precision)?
            } else {
                atomic(amount, 0)?
            };
            if exceeds(&value, &atomic(maximum, precision)?) {
                return Err(invalid("charge exceeds max_amount"));
            }
        }
        if let Some(maximum) = &self.max_atomic_amount
            && exceeds(&atomic(amount, 0)?, maximum)
        {
            return Err(invalid("charge exceeds max_atomic_amount"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inflow_mpp::Base64UrlJson;
    use serde_json::json;

    fn challenge(method: &str, request: Value) -> PaymentChallenge {
        PaymentChallenge::new(
            "id",
            "shop",
            method,
            "charge",
            Base64UrlJson::from_value(&request).unwrap(),
        )
    }

    #[test]
    fn exact_amounts_and_configuration_errors() {
        for value in ["", "-1", "1e3", "1.", ".1", "1.2.3", "NaN"] {
            assert!(decimal(value).is_err(), "{value}");
            assert!(atomic(value, 2).is_err(), "{value}");
        }
        assert_eq!(atomic("0001.2300", 2).unwrap(), "000123");
        assert!(atomic("1.231", 2).is_err());
        assert!(!exceeds("000", "0"));
        assert!(!exceeds("09", "10"));
        assert!(exceeds("10", "9"));
        assert!(exceeds("9007199254740993", "9007199254740992"));
        for (method, limits) in [
            (
                "inflow",
                MethodLimits {
                    max_atomic_amount: Some("1".into()),
                    ..Default::default()
                },
            ),
            (
                "inflow",
                MethodLimits {
                    max_amount_decimals: Some(2),
                    ..Default::default()
                },
            ),
            (
                "card",
                MethodLimits {
                    allowed_chain_ids: Some(vec![]),
                    ..Default::default()
                },
            ),
            (
                "card",
                MethodLimits {
                    currencies: Some(vec![]),
                    ..Default::default()
                },
            ),
            (
                "card",
                MethodLimits {
                    expected_recipients: Some(vec![]),
                    ..Default::default()
                },
            ),
            (
                "inflow",
                MethodLimits {
                    currencies: Some(vec!["".into()]),
                    ..Default::default()
                },
            ),
            (
                "tempo",
                MethodLimits {
                    allowed_chain_ids: Some(vec![u64::MAX]),
                    ..Default::default()
                },
            ),
            (
                "inflow",
                MethodLimits {
                    max_amount: Some("1".into()),
                    ..Default::default()
                },
            ),
            (
                "tempo",
                MethodLimits {
                    max_atomic_amount: Some("1".into()),
                    ..Default::default()
                },
            ),
            (
                "tempo",
                MethodLimits {
                    max_amount: Some("1".into()),
                    currencies: Some(vec![]),
                    ..Default::default()
                },
            ),
            (
                "card",
                MethodLimits {
                    max_amount: Some("0.001".into()),
                    ..Default::default()
                },
            ),
            (
                "card",
                MethodLimits {
                    max_amount: Some("bad".into()),
                    ..Default::default()
                },
            ),
            (
                "card",
                MethodLimits {
                    max_atomic_amount: Some("".into()),
                    ..Default::default()
                },
            ),
            (
                "card",
                MethodLimits {
                    max_atomic_amount: Some("1.5".into()),
                    ..Default::default()
                },
            ),
        ] {
            assert!(limits.validate(method).is_err());
        }
        MethodLimits::default().validate("inflow").unwrap();
        MethodLimits {
            max_atomic_amount: Some("0".into()),
            ..Default::default()
        }
        .validate("card")
        .unwrap();
    }

    #[test]
    fn decimal_limits_recipients_and_empty_lists() {
        let mut limits = MethodLimits {
            max_amount: Some("9007199254740992.50".into()),
            currencies: Some(vec!["USDC".into()]),
            ..Default::default()
        };
        limits.validate("inflow").unwrap();
        for (amount, ok) in [
            ("9007199254740992.5000", true),
            ("9007199254740992.5001", false),
            ("0", true),
        ] {
            assert_eq!(
                limits
                    .check(&challenge(
                        "inflow",
                        json!({"amount":amount,"currency":"USDC"})
                    ))
                    .is_ok(),
                ok
            );
        }
        assert!(
            limits
                .check(&challenge("inflow", json!({"amount":"1","currency":"USD"})))
                .is_err()
        );
        limits.expected_recipients = Some(vec![]);
        assert!(
            limits
                .check(&challenge(
                    "inflow",
                    json!({"amount":"1","currency":"USDC"})
                ))
                .is_err()
        );
        limits.expected_recipients = Some(vec!["12345678-1234-1234-1234-123456789abc".into()]);
        assert!(limits.check(&challenge("inflow", json!({"amount":"1","currency":"USDC","recipient":"12345678-1234-1234-1234-123456789abc"}))).is_ok());
        limits.currencies = Some(vec![]);
        assert!(
            limits
                .check(&challenge(
                    "inflow",
                    json!({"amount":"1","currency":"USDC"})
                ))
                .is_err()
        );
        let mut bad = challenge("inflow", json!({}));
        assert!(limits.check(&bad).is_err());
        bad.request = Base64UrlJson::from_raw("!");
        assert!(limits.check(&bad).is_err());
    }

    #[test]
    fn tempo_atomic_precision_chain_and_all_split_recipients() {
        let currency = "0xabcdef0000000000000000000000000000000000";
        let recipient = "0x1111111111111111111111111111111111111111";
        let limits = MethodLimits {
            max_amount: Some("0.50".into()),
            max_amount_decimals: Some(6),
            max_atomic_amount: Some("499999".into()),
            currencies: Some(vec![currency.to_uppercase()]),
            expected_recipients: Some(vec![recipient.into()]),
            allowed_chain_ids: Some(vec![42431]),
        };
        limits.validate("tempo").unwrap();
        let mut request = json!({"amount":"499999","currency":currency,"recipient":recipient,"methodDetails":{"chainId":42431,"splits":[{"recipient":recipient,"amount":"1"}]}});
        assert!(limits.check(&challenge("tempo", request.clone())).is_ok());
        request["amount"] = json!("500000");
        assert!(limits.check(&challenge("tempo", request.clone())).is_err());
        request["amount"] = json!("500001");
        assert!(limits.check(&challenge("tempo", request.clone())).is_err());
        request["amount"] = json!("1");
        request["methodDetails"]["splits"][0]["recipient"] = json!(currency);
        assert!(limits.check(&challenge("tempo", request.clone())).is_err());
        request["methodDetails"]["splits"] = json!([]);
        for chain in [Value::Null, json!(1), json!(1.5)] {
            request["methodDetails"]["chainId"] = chain;
            assert!(limits.check(&challenge("tempo", request.clone())).is_err());
        }
        request["methodDetails"] = json!({});
        assert!(limits.check(&challenge("tempo", request.clone())).is_err());
        let unrestricted = MethodLimits::default();
        assert!(unrestricted.check(&challenge("tempo", request)).is_ok());
    }
}
