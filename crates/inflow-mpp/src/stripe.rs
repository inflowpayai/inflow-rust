use crate::{Error, codec::invalid};
use serde_json::Value;

pub(crate) fn request(value: &Value) -> Result<(), Error> {
    let amount = value["amount"].as_str().unwrap_or_default();
    if amount.is_empty()
        || !amount.bytes().all(|b| b.is_ascii_digit())
        || !amount
            .parse::<u32>()
            .is_ok_and(|v| (50..=99_999_999).contains(&v))
        || value["currency"] != "usd"
    {
        return Err(invalid("Stripe amount or currency"));
    }
    if let Some(reference) = value.get("externalId")
        && reference
            .as_str()
            .is_none_or(|s| s.encode_utf16().count() > 255)
    {
        return Err(invalid("Stripe externalId"));
    }
    let details = &value["methodDetails"];
    if details["networkId"]
        .as_str()
        .is_none_or(|s| s.trim().is_empty())
        || !details["paymentMethodTypes"].as_array().is_some_and(|a| {
            !a.is_empty()
                && a.iter()
                    .all(|v| v.as_str().is_some_and(|s| !s.trim().is_empty()))
        })
    {
        return Err(invalid("Stripe method details"));
    }
    if let Some(metadata) = details.get("metadata") {
        let map = metadata
            .as_object()
            .filter(|m| m.len() <= 45)
            .ok_or_else(|| invalid("Stripe metadata"))?;
        for (key, value) in map {
            if key.trim().is_empty()
                || key.encode_utf16().count() > 40
                || key.contains(['[', ']'])
                || matches!(
                    key.as_str(),
                    "externalId"
                        | "inflowMppTransactionId"
                        | "mppChallengeId"
                        | "mppIntent"
                        | "mppMethod"
                        | "stripeNetworkProfile"
                )
                || value
                    .as_str()
                    .is_none_or(|s| s.encode_utf16().count() > 500)
            {
                return Err(invalid("Stripe metadata entry"));
            }
        }
    }
    Ok(())
}
