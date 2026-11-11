use crate::{Error, SettleResponse, VerifyRequest, invalid};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

/// Standard padded Base64 JSON for x402 HTTP headers. Does not sign or verify payment.
pub fn encode(value: &Value) -> String {
    STANDARD.encode(value.to_string())
}

pub fn decode(encoded: &str) -> Result<Value, Error> {
    let bytes = STANDARD.decode(encoded).map_err(|_| invalid("encoding"))?;
    serde_json::from_slice(&bytes).map_err(|_| invalid("JSON"))
}

pub const INFLOW_TRANSACTION: &str = "inflow-transaction";

/// Read the seller's Transaction UUID without substituting the settlement reference.
pub fn inflow_transaction_id(result: &SettleResponse) -> Option<&str> {
    if result.0["success"] != true {
        return None;
    }
    result.0["extensions"][INFLOW_TRANSACTION]["info"]["transactionId"]
        .as_str()
        .filter(|id| id.len() == 36 && uuid::Uuid::try_parse(id).is_ok())
}

/// Retains payload extensions and request fields without projecting through chain-specific types.
pub fn facilitator_request(payload: &Value, requirements: &Value) -> Result<VerifyRequest, Error> {
    if payload["x402Version"] != 2 {
        return Err(Error::new(
            "X402_VERSION_MISMATCH",
            "x402 version must be 2",
        ));
    }
    if !payload["payload"].is_object()
        || !payload["accepted"].is_object()
        || !requirements.is_object()
    {
        return Err(invalid("payment request"));
    }
    Ok(serde_json::from_value(json!({
        "x402Version": 2, "paymentPayload": payload, "paymentRequirements": requirements
    }))
    .expect("a JSON value is representable by the upstream raw JSON request"))
}
