use crate::{Error, codec::invalid};
use serde_json::Value;

fn require(ok: bool, field: &str) -> Result<(), Error> {
    if ok { Ok(()) } else { Err(invalid(field)) }
}

fn text(value: &Value, min: usize, max: usize) -> bool {
    value
        .as_str()
        .is_some_and(|s| (min..=max).contains(&s.encode_utf16().count()))
}

pub(crate) fn request(value: &Value) -> Result<(), Error> {
    let amount = value["amount"].as_str().unwrap_or_default();
    require(
        !amount.starts_with('0')
            && amount.bytes().all(|b| b.is_ascii_digit())
            && amount
                .parse::<u32>()
                .is_ok_and(|v| (50..=99_999_999).contains(&v)),
        "CARD amount",
    )?;
    require(value["currency"] == "usd", "CARD currency")?;
    require(text(&value["recipient"], 1, 255), "CARD recipient")?;
    if let Some(v) = value.get("description") {
        require(v.is_string(), "CARD description")?;
    }
    if let Some(v) = value.get("externalId") {
        require(text(v, 0, 255), "CARD externalId")?;
    }
    let details = &value["methodDetails"];
    require(
        details["acceptedNetworks"]
            .as_array()
            .is_some_and(|a| !a.is_empty() && a.iter().all(|v| v == "visa")),
        "CARD networks",
    )?;
    require(text(&details["merchantName"], 1, 255), "CARD merchant name")?;
    let key = &details["encryptionJwk"];
    require(
        key["kty"] == "RSA"
            && key["alg"] == "RSA-OAEP-256"
            && key["use"] == "enc"
            && text(&key["kid"], 1, usize::MAX),
        "CARD encryption key",
    )?;
    for field in ["n", "e"] {
        require(
            key[field].as_str().is_some_and(|s| {
                !s.is_empty()
                    && s.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            }),
            "CARD public key parameter",
        )?;
    }
    if let Some(v) = details.get("billingRequired") {
        require(v.is_boolean(), "CARD billing requirement")?;
    }
    Ok(())
}

pub(crate) fn payload(value: &Value) -> Result<(), Error> {
    require(
        text(&value["encryptedPayload"], 1, 16_384),
        "CARD encrypted payload",
    )?;
    require(value["network"] == "visa", "CARD network")?;
    for field in ["panLastFour", "panExpirationYear"] {
        require(
            value[field]
                .as_str()
                .is_some_and(|s| s.len() == 4 && s.bytes().all(|b| b.is_ascii_digit())),
            "CARD card digits",
        )?;
    }
    require(
        value["panExpirationMonth"].as_str().is_some_and(|s| {
            s.len() == 2
                && s.bytes().all(|b| b.is_ascii_digit())
                && s.parse::<u8>().is_ok_and(|v| (1..=12).contains(&v))
        }),
        "CARD expiration month",
    )?;
    for field in ["cardholderFullName", "paymentAccountReference"] {
        if let Some(v) = value.get(field) {
            require(v.is_string(), "CARD optional payload field")?;
        }
    }
    if let Some(address) = value.get("billingAddress") {
        require(address.is_object(), "CARD billing address")?;
        for field in ["line1", "line2", "city", "state", "zip", "countryCode"] {
            if let Some(v) = address.get(field) {
                require(v.is_string(), "CARD address field")?;
            }
        }
    }
    Ok(())
}
