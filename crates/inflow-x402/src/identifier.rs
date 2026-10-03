use crate::{Error, invalid};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const NAME: &str = "payment-identifier";

pub fn valid_payment_id(id: &str) -> bool {
    (16..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}

pub fn generate_payment_id(prefix: &str) -> Result<String, Error> {
    let id = format!("{prefix}{}", uuid::Uuid::new_v4().simple());
    if !valid_payment_id(&id) {
        return Err(invalid("payment identifier prefix"));
    }
    Ok(id)
}

pub fn identifier_declaration() -> Value {
    json!({"info":{"required":false},"schema":{
        "$schema":"https://json-schema.org/draft/2020-12/schema", "type":"object",
        "properties":{
            "id":{"type":"string","minLength":16,"maxLength":128,"pattern":"^[a-zA-Z0-9_-]+$"},
            "required":{"type":"boolean"}
        },"required":["required"]
    }})
}

pub fn identifier_entry(declaration: &Value, id: &str) -> Option<Value> {
    if !valid_payment_id(id) || !valid_declaration(declaration) {
        return None;
    }
    let mut entry = declaration.clone();
    entry["info"]["id"] = json!(id);
    Some(entry)
}

fn valid_declaration(value: &Value) -> bool {
    let schema = &value["schema"];
    let expected = identifier_declaration();
    value["info"]["required"].is_boolean()
        && ["$schema", "type", "required"]
            .iter()
            .all(|key| schema[*key] == expected["schema"][*key])
        && ["type", "minLength", "maxLength", "pattern"]
            .iter()
            .all(|key| {
                schema["properties"]["id"][*key] == expected["schema"]["properties"]["id"][*key]
            })
        && schema["properties"]["required"]["type"] == "boolean"
}

pub(crate) fn ensure_identifier(payload: &Value) -> Result<Value, Error> {
    let existing = &payload["extensions"][NAME];
    if valid_declaration(existing)
        && existing["info"]["id"]
            .as_str()
            .is_some_and(valid_payment_id)
    {
        return Ok(payload.clone());
    }
    let data = &payload["payload"];
    let material = ["transactionId", "transaction", "signature"]
        .iter()
        .find_map(|key| {
            data[*key]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(|s| format!("{key}:{s}"))
        })
        .unwrap_or_else(|| format!("payload:{data}"));
    let hash: String = Sha256::digest(material.as_bytes())
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let mut entry = identifier_declaration();
    entry["info"]["id"] = json!(format!("pay_{hash}"));
    let mut result = payload.clone();
    if result.get("extensions").is_none() {
        result["extensions"] = json!({});
    }
    let extensions = result["extensions"]
        .as_object_mut()
        .ok_or_else(|| invalid("extensions"))?;
    extensions.insert(NAME.into(), entry);
    Ok(result)
}
