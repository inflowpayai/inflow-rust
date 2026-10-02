use crate::Error;
use crate::codec::{invalid, nonempty, timestamp};
use serde_json::Value;

/// Validate method-specific request fields without authorizing or making a payment.
/// Unknown object members are retained by the caller; this function does not mutate input.
pub fn validate_request(method: &str, intent: &str, request: &Value) -> Result<(), Error> {
    let amount = nonempty(&request["amount"], "amount")?;
    match (method, intent) {
        ("inflow", "charge" | "subscription") => {
            check(decimal(amount), "decimal amount")?;
            nonempty(&request["currency"], "currency")?;
            optional(request, "recipient", guid)?;
            if let Some(details) = request.get("methodDetails") {
                check(details.is_object(), "methodDetails")?;
                optional(details, "rail", |v| matches!(v, "balance" | "instrument"))?;
                optional(details, "instrumentId", guid)?;
            }
            if intent == "subscription" {
                check(
                    !amount.starts_with('-') && amount.bytes().any(|b| matches!(b, b'1'..=b'9')),
                    "subscription amount",
                )?;
                let unit = nonempty(&request["periodUnit"], "periodUnit")?;
                check(
                    matches!(
                        unit,
                        "minute" | "hour" | "day" | "week" | "month" | "quarter" | "year"
                    ),
                    "periodUnit",
                )?;
                let count = request["periodCount"]
                    .as_f64()
                    .ok_or_else(|| invalid("periodCount"))?;
                check(
                    (1.0..=9_007_199_254_740_991.0).contains(&count)
                        && count.fract() == 0.0
                        && (unit != "minute" || count >= 5.0),
                    "periodCount",
                )?;
                let expires = nonempty(&request["subscriptionExpires"], "subscriptionExpires")?;
                check(expires.ends_with('Z'), "subscriptionExpires")?;
                timestamp(expires)?;
                optional(request, "externalId", |v| {
                    !v.trim().is_empty() && v.encode_utf16().count() <= 128
                })?;
            }
        }
        ("tempo", "charge") => {
            check(integer(amount), "base-unit amount")?;
            optional(request, "currency", address)?;
            optional(request, "recipient", address)?;
            optional(request, "description", |_| true)?;
            optional(request, "externalId", |_| true)?;
            if let Some(details) = request.get("methodDetails") {
                check(details.is_object(), "methodDetails")?;
                if let Some(chain) = details.get("chainId") {
                    check(chain.is_number(), "chainId")?;
                }
                if let Some(payer) = details.get("feePayer") {
                    check(payer.is_boolean(), "feePayer")?;
                }
                optional(details, "memo", bytes32)?;
                if let Some(splits) = details.get("splits") {
                    for split in splits.as_array().ok_or_else(|| invalid("splits"))? {
                        check(
                            integer(nonempty(&split["amount"], "split amount")?),
                            "split amount",
                        )?;
                        check(
                            address(nonempty(&split["recipient"], "split recipient")?),
                            "split recipient",
                        )?;
                        optional(split, "memo", bytes32)?;
                    }
                }
                if let Some(modes) = details.get("supportedModes") {
                    for mode in modes.as_array().ok_or_else(|| invalid("supportedModes"))? {
                        check(
                            matches!(mode.as_str(), Some("pull" | "push")),
                            "supportedModes",
                        )?;
                    }
                }
            }
        }
        _ => {
            return Err(Error::new(
                "UNSUPPORTED_MPP_METHOD",
                "unsupported MPP method or intent",
            ));
        }
    }
    Ok(())
}

pub fn validate_payload(method: &str, payload: &Value) -> Result<(), Error> {
    check(payload.is_object(), "payload")?;
    match method {
        "inflow" => {}
        "tempo" => {
            optional(payload, "transactionId", |_| true)?;
            let field = match payload["type"].as_str() {
                Some("hash") => "hash",
                Some("transaction" | "proof") => "signature",
                _ => return Err(invalid("Tempo payload type")),
            };
            check(hex(nonempty(&payload[field], field)?), field)?;
            optional(payload, "hash", hex)?;
            optional(payload, "signature", hex)?;
        }
        _ => {
            return Err(Error::new(
                "UNSUPPORTED_MPP_METHOD",
                "unsupported MPP method",
            ));
        }
    }
    Ok(())
}

fn check(condition: bool, field: &str) -> Result<(), Error> {
    if condition {
        Ok(())
    } else {
        Err(invalid(field))
    }
}

fn optional(value: &Value, field: &str, valid: impl FnOnce(&str) -> bool) -> Result<(), Error> {
    if let Some(value) = value.get(field) {
        check(valid(nonempty(value, field)?), field)?;
    }
    Ok(())
}

fn decimal(value: &str) -> bool {
    let value = value.strip_prefix('-').unwrap_or(value);
    let mut parts = value.split('.');
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    digits(parts.next().unwrap_or_default())
        && parts.next().is_none_or(digits)
        && parts.next().is_none()
}

fn integer(value: &str) -> bool {
    value == "0"
        || (!value.starts_with('0')
            && !value.is_empty()
            && value.bytes().all(|b| b.is_ascii_digit()))
}

fn hex(value: &str) -> bool {
    value
        .strip_prefix("0x")
        .is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn address(value: &str) -> bool {
    value.len() == 42 && hex(value)
}
fn bytes32(value: &str) -> bool {
    value.len() == 66 && hex(value)
}

fn guid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}
