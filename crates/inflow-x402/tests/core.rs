use inflow_x402::*;
use serde_json::{Value, json};

#[test]
fn shared_core_cases() {
    // Verbatim x402-core vectors from inflow-specs d79cc3a; the adapter only maps public calls.
    let cases: Vec<Value> = serde_json::from_str(include_str!("data/core-cases.json")).unwrap();
    assert_eq!(cases.len(), 14);
    for case in cases {
        let input = &case["input"];
        let result = match case["operation"].as_str().unwrap() {
            "x402.core.identifier-valid" => {
                json!(input["value"].as_str().is_some_and(valid_payment_id))
            }
            "x402.core.identifier-declaration" => identifier_declaration(),
            "x402.core.identifier-entry" => {
                identifier_entry(&input["declaration"], input["payment_id"].as_str().unwrap())
                    .unwrap_or(Value::Null)
            }
            operation => panic!("unhandled {operation}"),
        };
        assert_eq!(json!({"result":result}), case["expect"], "{}", case["id"]);
    }
}

#[test]
fn identifiers_preserve_schema_and_information_without_mutation() {
    let mut declaration = identifier_declaration();
    declaration["info"]["merchant"] = json!("shop");
    declaration["schema"]["title"] = json!("Payment identifier");
    declaration["schema"]["properties"]["extra"] = json!({"type":"string"});
    let before = declaration.clone();
    let entry = identifier_entry(&declaration, "pay_0123456789abcdef").unwrap();
    assert_eq!(declaration, before);
    assert_eq!(entry["schema"], before["schema"]);
    assert_eq!(entry["info"]["merchant"], "shop");
    for pointer in [
        "/info/required",
        "/schema/$schema",
        "/schema/type",
        "/schema/required",
        "/schema/properties/id/type",
        "/schema/properties/id/minLength",
        "/schema/properties/id/maxLength",
        "/schema/properties/id/pattern",
        "/schema/properties/required/type",
    ] {
        let mut malformed = declaration.clone();
        *malformed.pointer_mut(pointer).unwrap() = Value::Null;
        assert!(
            identifier_entry(&malformed, "pay_0123456789abcdef").is_none(),
            "{pointer}"
        );
    }
    assert!(identifier_entry(&Value::Null, "pay_0123456789abcdef").is_none());
    for prefix in ["", "pay_", &"a".repeat(96)] {
        let first = generate_payment_id(prefix).unwrap();
        assert!(valid_payment_id(&first));
        assert!(first.starts_with(prefix));
        assert_ne!(first, generate_payment_id(prefix).unwrap());
    }
    for prefix in ["!", "é", &"a".repeat(97)] {
        assert!(generate_payment_id(prefix).is_err());
    }
}

#[test]
fn wire_retains_all_fields_and_uses_standard_base64() {
    let value = json!({"unicode":"你好ÿ","null":null,"amount":"900719925474099300000","extensions":{"future":{"x":1}}});
    assert_eq!(decode(&encode(&value)).unwrap(), value);
    assert_eq!(encode(&json!({})), "e30=");
    for encoded in ["!", "e30", "e30_", "eA=="] {
        assert!(decode(encoded).is_err());
    }
    let payload = json!({"x402Version":2,"accepted":{},"payload":{"signature":"secret"},"extensions":{"future":null}});
    let requirements = json!({"scheme":"future","extra":null});
    let request = facilitator_request(&payload, &requirements).unwrap();
    let value: Value = serde_json::from_str(request.as_str()).unwrap();
    assert_eq!(value["paymentPayload"], payload);
    assert_eq!(value["paymentRequirements"], requirements);
    for version in [json!(1), json!(3), json!("2"), Value::Null] {
        let mut bad = payload.clone();
        bad["x402Version"] = version;
        assert_eq!(
            facilitator_request(&bad, &requirements).unwrap_err().code,
            "X402_VERSION_MISMATCH"
        );
    }
    for key in ["payload", "accepted"] {
        let mut bad = payload.clone();
        bad[key] = Value::Null;
        assert!(facilitator_request(&bad, &requirements).is_err());
    }
    assert!(facilitator_request(&payload, &Value::Null).is_err());
    let verification = json!({"isValid":false,"invalidReason":"declined","invalidMessage":"Details","extensions":{"a":1}});
    let decoded: VerifyResponse = serde_json::from_value(verification.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), verification);
    let settlement = json!({"success":false,"errorReason":"failed","errorMessage":"Details","transaction":"","amount":"2","extensions":{"a":1}});
    let decoded: SettleResponse = serde_json::from_value(settlement.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), settlement);
    assert_eq!(
        (
            X402_VERSION,
            NETWORK_INFLOW,
            PAYMENT_REQUIRED,
            PAYMENT_SIGNATURE,
            PAYMENT_RESPONSE
        ),
        (
            2,
            "inflow:1",
            "PAYMENT-REQUIRED",
            "PAYMENT-SIGNATURE",
            "PAYMENT-RESPONSE"
        )
    );
    let requirements: PaymentRequirements=serde_json::from_value(json!({"scheme":"balance","network":"inflow:1","amount":"1000000000000000000","asset":"USD","payTo":"seller","maxTimeoutSeconds":60})).unwrap();
    assert_eq!(requirements.amount, "1000000000000000000");
    assert_eq!(serde_json::to_value(X402Version2).unwrap(), 2);
}
