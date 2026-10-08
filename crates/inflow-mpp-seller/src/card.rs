use crate::{Error, Method};
use inflow_mpp::validate_request;
use serde_json::{Value, json};

pub(crate) fn prepare(method: Method, config: &Value, input: Value) -> Result<Value, Error> {
    let name = method.name();
    let capability = config["supportedMethods"]
        .as_array()
        .and_then(|a| a.iter().find(|v| v["id"] == name))
        .ok_or_else(|| unavailable(method))?;
    for (field, expected) in [
        ("supportedCurrencies", "USD"),
        ("supportedIntents", "charge"),
    ] {
        if !capability[field]
            .as_array()
            .is_some_and(|v| v.iter().any(|v| v == expected))
        {
            return Err(unavailable(method));
        }
    }
    let details = &capability["methodDetails"];
    let mut configured = if method == Method::Card {
        json!({"amount":"100", "currency":"usd", "recipient":details["recipient"],
            "methodDetails":{"acceptedNetworks":details["acceptedNetworks"],
                "merchantName":details["merchantName"], "encryptionJwk":details["encryptionJwk"]}})
    } else {
        json!({"amount":"100", "currency":"usd", "methodDetails":{
            "networkId":details["networkId"], "paymentMethodTypes":details["paymentMethodTypes"]}})
    };
    if method == Method::Card
        && let Some(billing) = details.get("billingRequired")
    {
        configured["methodDetails"]["billingRequired"] = billing.clone();
    }
    validate_request(name, "charge", &configured).map_err(|_| unavailable(method))?;
    configured["amount"] = json!(cents(&input["amount"])?);
    if method == Method::Card {
        if let Some(billing) = input.get("billingRequired") {
            configured["methodDetails"]["billingRequired"] = billing.clone();
        }
    } else if let Some(metadata) = input.get("metadata") {
        configured["methodDetails"]["metadata"] = metadata.clone();
    }
    for field in ["description", "externalId"] {
        if let Some(value) = input.get(field) {
            configured[field] = value.clone();
        }
    }
    validate_request(name, "charge", &configured)?;
    Ok(configured)
}

fn cents(value: &Value) -> Result<String, Error> {
    let amount = value
        .as_str()
        .ok_or_else(|| invalid("USD amount must be a decimal string"))?;
    let (whole, fraction) = amount.split_once('.').unwrap_or((amount, ""));
    if whole.is_empty()
        || whole.len() > 6
        || (whole.len() > 1 && whole.starts_with('0'))
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || fraction.len() > 2
        || (amount.contains('.') && fraction.is_empty())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(invalid("USD amount must have at most two decimal places"));
    }
    // Validated lengths bound this arithmetic to six whole digits and two fractional digits.
    let digits = |s: &str| s.bytes().fold(0_u32, |n, b| n * 10 + u32::from(b - b'0'));
    let cents = digits(whole) * 100 + digits(fraction) * if fraction.len() == 1 { 10 } else { 1 };
    if !(50..=99_999_999).contains(&cents) {
        return Err(invalid("USD amount must be 0.50 through 999999.99"));
    }
    Ok(cents.to_string())
}
fn invalid(message: &str) -> Error {
    Error::new("INVALID_MPP_DATA", message)
}
fn unavailable(method: Method) -> Error {
    Error::new(
        if method == Method::Stripe {
            "MPP_STRIPE_UNAVAILABLE"
        } else {
            "MPP_CARD_UNAVAILABLE"
        },
        "Seller payment capability is unavailable",
    )
}
