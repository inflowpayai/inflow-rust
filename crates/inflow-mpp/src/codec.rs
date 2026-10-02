use crate::{Base64UrlJson, Error, PaymentChallenge, Receipt};
use mpp::protocol::core::base64url_decode;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A credential retaining the complete challenge, including its display description.
/// Encoding and decoding do not verify payment or establish settlement.
#[derive(Clone, Serialize, Deserialize)]
pub struct Credential {
    // mpp 0.14's ChallengeEcho drops description; upstream fix: mpp-rs PR490.
    pub challenge: PaymentChallenge,
    pub payload: Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// Encode canonical JSON, omitting null object members as the InFlow API does.
/// Null array elements are retained. Amounts must remain strings.
pub fn encode(value: &Value) -> Result<String, Error> {
    let value = without_null_members(value);
    Base64UrlJson::from_value(&value)
        .map(|value| value.raw().to_owned())
        .map_err(encoding_error)
}

fn without_null_members(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(_, value)| !value.is_null())
                .map(|(key, value)| (key.clone(), without_null_members(value)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(without_null_members).collect()),
        value => value.clone(),
    }
}

pub fn decode(encoded: &str) -> Result<Value, Error> {
    let bytes = base64url_decode(encoded).map_err(encoding_error)?;
    serde_json::from_slice(&bytes).map_err(json_error)
}

pub fn decode_credential(encoded: &str) -> Result<Credential, Error> {
    let value = decode(encoded)?;
    if let Some(source) = value.get("source")
        && !source.is_string()
    {
        return Err(invalid("credential source"));
    }
    let credential: Credential = serde_json::from_value(value).map_err(json_error)?;
    validate_challenge(&credential.challenge)?;
    Ok(credential)
}

pub fn encode_credential(credential: &Credential) -> Result<String, Error> {
    validate_challenge(&credential.challenge)?;
    // Do not decode/re-encode request or opaque: their original bytes are challenge-bound.
    encode(&serde_json::to_value(credential).map_err(json_error)?)
}

pub fn decode_receipt(encoded: &str) -> Result<Receipt, Error> {
    let value = decode(encoded)?;
    for field in ["method", "reference", "timestamp"] {
        nonempty(&value[field], field)?;
    }
    if value["status"] != "success" {
        return Err(invalid("receipt status"));
    }
    timestamp(value["timestamp"].as_str().unwrap_or_default())?;
    for field in ["challengeId", "subscriptionId"] {
        if let Some(value) = value.get(field) {
            nonempty(value, field)?;
        }
    }
    if let Some(settlement) = value.get("settlement") {
        nonempty(&settlement["amount"], "settlement amount")?;
        nonempty(&settlement["currency"], "settlement currency")?;
    }
    serde_json::from_value(value).map_err(|_| invalid("receipt"))
}

pub(crate) fn validate_challenge(challenge: &PaymentChallenge) -> Result<(), Error> {
    for value in [
        &challenge.id,
        &challenge.realm,
        challenge.method.as_str(),
        challenge.intent.as_str(),
        challenge.request.raw(),
    ] {
        if value.is_empty() {
            return Err(invalid("empty challenge field"));
        }
    }
    if let Some(header) = &challenge.header
        && !header.eq_ignore_ascii_case("Payment-Authorization")
    {
        return Err(invalid("credential header"));
    }
    Ok(())
}

pub(crate) fn timestamp(value: &str) -> Result<(), Error> {
    time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
        .map(|_| ())
        .map_err(|_| invalid("timestamp"))
}

pub(crate) fn nonempty<'a>(value: &'a Value, field: &str) -> Result<&'a str, Error> {
    value
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| invalid(field))
}

pub(crate) fn invalid(field: &str) -> Error {
    Error::new("INVALID_MPP_DATA", format!("invalid MPP {field}"))
}

fn encoding_error(_: mpp::MppError) -> Error {
    invalid("encoding")
}

fn json_error(_: serde_json::Error) -> Error {
    invalid("JSON")
}
