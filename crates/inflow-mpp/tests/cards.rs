use inflow_mpp::{validate_payload, validate_request};
use serde_json::{Value, json};

#[test]
fn card_wire_fields_match_node_without_rewriting_payloads() {
    let request = json!({"amount":"125","currency":"usd","recipient":"shop","externalId":"","description":"read",
        "methodDetails":{"acceptedNetworks":["visa"],"merchantName":"Shop","billingRequired":false,
            "encryptionJwk":{"kty":"RSA","alg":"RSA-OAEP-256","use":"enc","kid":"key","n":"abc-_","e":"AQAB"}}});
    validate_request("card", "charge", &request).unwrap();
    for (path, values) in [
        (
            "/amount",
            vec![
                json!(""),
                json!("0"),
                json!("49"),
                json!("100000000"),
                json!("0125"),
                json!("1e2"),
                json!(125),
            ],
        ),
        ("/currency", vec![json!("USD")]),
        ("/recipient", vec![json!(""), json!("x".repeat(256))]),
        ("/description", vec![json!(false)]),
        ("/externalId", vec![json!("x".repeat(256)), Value::Null]),
        (
            "/methodDetails/acceptedNetworks",
            vec![json!([]), json!(["mastercard"]), Value::Null],
        ),
        (
            "/methodDetails/merchantName",
            vec![json!(""), json!("x".repeat(256))],
        ),
        ("/methodDetails/encryptionJwk/kty", vec![json!("EC")]),
        ("/methodDetails/encryptionJwk/alg", vec![json!("RSA-OAEP")]),
        ("/methodDetails/encryptionJwk/use", vec![json!("sig")]),
        ("/methodDetails/encryptionJwk/kid", vec![json!("")]),
        (
            "/methodDetails/encryptionJwk/n",
            vec![json!(""), json!("abc="), Value::Null],
        ),
        ("/methodDetails/encryptionJwk/e", vec![json!("=")]),
        ("/methodDetails/billingRequired", vec![json!("true")]),
    ] {
        for value in values {
            let mut bad = request.clone();
            *bad.pointer_mut(path).unwrap() = value;
            assert!(validate_request("card", "charge", &bad).is_err(), "{path}");
        }
    }
    let payload = json!({"encryptedPayload":"opaque","network":"visa","panLastFour":"1234","panExpirationMonth":"01","panExpirationYear":"2030",
        "cardholderFullName":"A","paymentAccountReference":"ref","billingAddress":{"line1":"","line2":"","city":"","state":"","zip":"","countryCode":"US","extension":true},"extension":true});
    let copy = payload.clone();
    validate_payload("card", &payload).unwrap();
    let mut empty_address = payload.clone();
    empty_address["billingAddress"] = json!({});
    validate_payload("card", &empty_address).unwrap();
    assert_eq!(payload, copy);
    for (path, value) in [
        ("/encryptedPayload", json!("")),
        ("/encryptedPayload", json!("x".repeat(16_385))),
        ("/network", json!("other")),
        ("/panLastFour", json!("12x4")),
        ("/panExpirationYear", json!("203")),
        ("/panExpirationMonth", json!("00")),
        ("/panExpirationMonth", json!("13")),
        ("/panExpirationMonth", json!("1")),
        ("/cardholderFullName", Value::Null),
        ("/paymentAccountReference", json!(false)),
        ("/billingAddress", json!([])),
        ("/billingAddress/zip", json!(123)),
    ] {
        let mut bad = payload.clone();
        *bad.pointer_mut(path).unwrap() = value;
        assert!(validate_payload("card", &bad).is_err(), "{path}");
    }
}

#[test]
fn stripe_wire_amounts_metadata_and_tokens() {
    let request = json!({"amount":"125","currency":"usd","externalId":"","methodDetails":{"networkId":"profile","paymentMethodTypes":["card","link"],"metadata":{"order":""}}});
    validate_request("stripe", "charge", &request).unwrap();
    for (path, values) in [
        (
            "/amount",
            vec![
                json!("0"),
                json!("49"),
                json!("100000000"),
                json!("1.25"),
                json!(""),
                json!("9999999999999999999"),
            ],
        ),
        ("/currency", vec![json!("USD")]),
        ("/externalId", vec![json!("x".repeat(256)), Value::Null]),
        ("/methodDetails/networkId", vec![json!(" "), Value::Null]),
        (
            "/methodDetails/paymentMethodTypes",
            vec![json!([]), json!([""]), json!([null]), Value::Null],
        ),
        (
            "/methodDetails/metadata",
            vec![
                json!([]),
                json!({" ":"v"}),
                json!({"a[b]":"v"}),
                json!({"order":null}),
                json!({"order":"x".repeat(501)}),
            ],
        ),
    ] {
        for value in values {
            let mut bad = request.clone();
            *bad.pointer_mut(path).unwrap() = value;
            assert!(
                validate_request("stripe", "charge", &bad).is_err(),
                "{path}"
            );
        }
    }
    for key in [
        "externalId",
        "inflowMppTransactionId",
        "mppChallengeId",
        "mppIntent",
        "mppMethod",
        "stripeNetworkProfile",
        &"x".repeat(41),
    ] {
        let mut bad = request.clone();
        bad["methodDetails"]["metadata"] = json!({key:"v"});
        assert!(validate_request("stripe", "charge", &bad).is_err());
    }
    let mut bad = request.clone();
    bad["methodDetails"]["metadata"] =
        Value::Object((0..46).map(|i| (format!("key{i}"), json!("v"))).collect());
    assert!(validate_request("stripe", "charge", &bad).is_err());
    validate_payload("stripe", &json!({"spt":"opaque"})).unwrap();
    validate_payload("stripe", &json!({"spt":"opaque","externalId":""})).unwrap();
    assert!(validate_payload("stripe", &json!({"spt":""})).is_err());
    assert!(validate_payload("stripe", &json!({"spt":"opaque","externalId":null})).is_err());
}
