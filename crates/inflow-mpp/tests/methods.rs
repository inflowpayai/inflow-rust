use inflow_mpp::{validate_payload, validate_request};
use serde_json::{Value, json};

#[test]
fn inflow_amounts_and_optional_fields_match_node_schemas() {
    for amount in [
        "0",
        "-1",
        "0001.20",
        "0.00000000000000000001",
        "9999999999999999999999999999999999",
    ] {
        validate_request(
            "inflow",
            "charge",
            &json!({"amount":amount,"currency":"USDC"}),
        )
        .unwrap();
    }
    for amount in ["", "1e3", "1.", ".2", "-", "+1", "1.2.3", "1x", "1.x", " 1"] {
        assert!(
            validate_request(
                "inflow",
                "charge",
                &json!({"amount":amount,"currency":"USDC"})
            )
            .is_err(),
            "{amount}"
        );
    }
    let full = json!({"amount":"1.5","currency":"USD","recipient":"11111111-1111-1111-1111-111111111111","methodDetails":{"rail":"instrument","instrumentId":"33333333-3333-3333-3333-333333333333"},"extension":true});
    let copy = full.clone();
    validate_request("inflow", "charge", &full).unwrap();
    assert_eq!(full, copy);
    for (path, value) in [
        ("/currency", json!("")),
        ("/amount", json!(1)),
        ("/recipient", json!("bad")),
        ("/recipient", json!("11111111x1111-1111-1111-111111111111")),
        ("/recipient", json!("g1111111-1111-1111-1111-111111111111")),
        ("/methodDetails/rail", json!("blockchain")),
        ("/methodDetails/instrumentId", json!(false)),
        ("/methodDetails", json!([])),
    ] {
        let mut request = full.clone();
        *request.pointer_mut(path).unwrap() = value;
        assert!(
            validate_request("inflow", "charge", &request).is_err(),
            "{path}"
        );
    }
    validate_request(
        "inflow",
        "charge",
        &json!({"amount":"1","currency":"USDC","methodDetails":{"rail":"balance"}}),
    )
    .unwrap();
    assert!(validate_request("other", "charge", &full).is_err());
    assert!(validate_request("inflow", "session", &full).is_err());
    assert!(validate_request("inflow", "charge", &json!(null)).is_err());
}

#[test]
fn subscription_terms() {
    let full = json!({"amount":"1","currency":"USDC","periodUnit":"minute","periodCount":5,"subscriptionExpires":"2030-01-01T00:00:00Z","externalId":"invoice"});
    validate_request("inflow", "subscription", &full).unwrap();
    let mut integral_float = full.clone();
    integral_float["periodCount"] = json!(5.0);
    validate_request("inflow", "subscription", &integral_float).unwrap();
    for unit in ["hour", "day", "week", "month", "quarter", "year"] {
        let mut v = full.clone();
        v["periodUnit"] = json!(unit);
        v["periodCount"] = json!(1);
        v.as_object_mut().unwrap().remove("externalId");
        validate_request("inflow", "subscription", &v).unwrap();
    }
    for (field, value) in [
        ("amount", json!("0.000")),
        ("amount", json!("-1")),
        ("periodUnit", json!("second")),
        ("periodUnit", json!(null)),
        ("periodCount", json!(4)),
        ("periodCount", json!(0)),
        ("periodCount", json!(1.5)),
        ("periodCount", json!(null)),
        ("periodCount", json!(9007199254740992_u64)),
        ("subscriptionExpires", json!("badZ")),
        ("subscriptionExpires", json!("2030-01-01T00:00:00+00:00")),
        ("subscriptionExpires", json!(null)),
        ("externalId", json!(" ")),
        ("externalId", json!("x".repeat(129))),
    ] {
        let mut v = full.clone();
        v[field] = value;
        assert!(
            validate_request("inflow", "subscription", &v).is_err(),
            "{field}"
        );
    }
}

#[test]
fn tempo_units_and_field_constraints() {
    let address = format!("0x{}", "a".repeat(40));
    let memo = format!("0x{}", "b".repeat(64));
    let full = json!({"amount":"10000000000000000000000000000000","currency":address,"recipient":address,"description":"request","externalId":"invoice","methodDetails":{"chainId":42431,"feePayer":false,"memo":memo,"splits":[{"amount":"3","recipient":address,"memo":memo}],"supportedModes":["pull","push"]}});
    validate_request("tempo", "charge", &full).unwrap();
    validate_request("tempo", "charge", &json!({"amount":"0"})).unwrap();
    validate_request("tempo", "charge", &json!({"amount":"1","methodDetails":{}})).unwrap();
    for (path, value) in [
        ("/amount", json!("01")),
        ("/amount", json!("-1")),
        ("/amount", json!("1.5")),
        ("/currency", json!("USD")),
        ("/recipient", json!(format!("0x{}", "z".repeat(40)))),
        ("/description", json!("")),
        ("/methodDetails", json!(false)),
        ("/methodDetails/chainId", json!("1")),
        ("/methodDetails/feePayer", json!("false")),
        ("/methodDetails/memo", json!("0x11")),
        ("/methodDetails/splits", json!({})),
        ("/methodDetails/splits/0/amount", json!("01")),
        ("/methodDetails/splits/0/recipient", json!("wrong")),
        ("/methodDetails/splits/0/memo", json!("0x")),
        ("/methodDetails/supportedModes", json!({})),
        ("/methodDetails/supportedModes/0", json!("other")),
    ] {
        let mut v = full.clone();
        *v.pointer_mut(path).unwrap() = value;
        assert!(validate_request("tempo", "charge", &v).is_err(), "{path}");
    }
}

#[test]
fn payload_proof_type_selects_its_required_field() {
    validate_payload(
        "inflow",
        &json!({"transactionId":42,"arbitrary":{"field":true}}),
    )
    .unwrap();
    for kind in ["transaction", "proof", "hash"] {
        let mut payload =
            json!({"type":kind,"signature":"0xab","hash":"0xcd","transactionId":"tx"});
        validate_payload("tempo", &payload).unwrap();
        let field = if kind == "hash" { "hash" } else { "signature" };
        payload.as_object_mut().unwrap().remove(field);
        assert!(validate_payload("tempo", &payload).is_err());
    }
    for payload in [
        json!([]),
        json!({"type":"other"}),
        json!({"type":"hash","hash":"0x"}),
        json!({"type":"hash","hash":"0xGG"}),
        json!({"type":"hash","hash":"0xab","signature":"no"}),
        json!({"type":"proof","signature":"0xa","transactionId":3}),
    ] {
        assert!(validate_payload("tempo", &payload).is_err());
    }
    assert!(validate_payload("unknown", &Value::Object(Default::default())).is_err());
}
