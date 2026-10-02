use inflow_mpp::*;
use serde_json::{Value, json};

fn challenge() -> PaymentChallenge {
    PaymentChallenge::new(
        "id",
        "shop",
        "inflow",
        "charge",
        Base64UrlJson::from_raw("e30"),
    )
}

#[test]
fn canonical_bytes_match_node_and_server_vector() {
    let value = json!({"recipient":"11111111-1111-1111-1111-111111111111", "methodDetails":{"rail":"balance"},"currency":"USDC","amount":"10.5"});
    let expected = "eyJhbW91bnQiOiIxMC41IiwiY3VycmVuY3kiOiJVU0RDIiwibWV0aG9kRGV0YWlscyI6eyJyYWlsIjoiYmFsYW5jZSJ9LCJyZWNpcGllbnQiOiIxMTExMTExMS0xMTExLTExMTEtMTExMS0xMTExMTExMTExMTEifQ";
    assert_eq!(encode(&value).unwrap(), expected);
    assert_eq!(decode(expected).unwrap(), value);
    let value = json!({"z":null,"a":[null,{"drop":null,"keep":true}], "number":1e-7});
    assert_eq!(
        decode(&encode(&value).unwrap()).unwrap(),
        json!({"a":[null,{"keep":true}],"number":1e-7})
    );
    assert!(value.get("z").is_some());
    // UTF-16 key order differs from Unicode scalar order for these two characters.
    let encoded = encode(&json!({"\u{e000}": 1, "\u{10000}":2})).unwrap();
    let bytes = mpp::protocol::core::base64url_decode(&encoded).unwrap();
    assert_eq!(String::from_utf8(bytes).unwrap(), "{\"𐀀\":2,\"\":1}");
    for bad in ["a", "!!!!", "bm90IGpzb24", "_w"] {
        assert!(decode(bad).is_err(), "{bad}");
    }
}

#[test]
fn credentials_preserve_challenge_and_open_payload_without_reencoding() {
    let mut challenge = challenge();
    challenge.request = Base64UrlJson::from_raw("eyJ6IjogMSwgImEiOiAyfQ==");
    challenge.description = Some("display text".into());
    challenge.opaque = Some(Base64UrlJson::from_raw("eyJ4IjogInkifQ=="));
    challenge.digest = Some("sha-256=:abc=:".into());
    challenge.expires = Some("2030-01-01T00:00:00Z".into());
    challenge.header = Some("Payment-Authorization".into());
    let explicit_null = mpp::protocol::core::base64url_encode(
        json!({"challenge":challenge,"payload":{},"source":null})
            .to_string()
            .as_bytes(),
    );
    assert!(decode_credential(&explicit_null).is_err());
    for source in [None, Some("did:example:payer".into()), Some(String::new())] {
        let credential = Credential {
            challenge: challenge.clone(),
            payload: json!({"signature":"synthetic","custom":{"trace":true}})
                .as_object()
                .unwrap()
                .clone(),
            source,
        };
        let value = serde_json::to_value(&credential).unwrap();
        let decoded = decode_credential(&encode_credential(&credential).unwrap()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), value);
    }
    for value in [
        json!(null),
        json!([]),
        json!({"challenge":{},"payload":{}}),
        json!({"challenge":challenge,"payload":[]}),
        json!({"challenge":challenge,"payload":{},"source":3}),
    ] {
        assert!(decode_credential(&encode(&value).unwrap()).is_err());
    }
    let mut value = json!({"challenge":challenge,"payload":{}});
    for field in ["id", "realm", "method", "intent", "request"] {
        let original = value["challenge"][field].clone();
        value["challenge"][field] = json!("");
        assert!(decode_credential(&encode(&value).unwrap()).is_err());
        value["challenge"][field] = original;
    }
    let bad = Credential {
        challenge: PaymentChallenge {
            header: Some("Cookie".into()),
            ..challenge
        },
        payload: Default::default(),
        source: None,
    };
    assert!(encode_credential(&bad).is_err());
}

#[test]
fn receipt_extensions_and_rejection() {
    let receipt = json!({"method":"inflow","reference":"ref","status":"success","timestamp":"2026-01-01T00:00:00Z","challengeId":"id","subscriptionId":"sub","settlement":{"amount":"0.1234567890123456789","currency":"USDC"},"extra":{"a":1}});
    let decoded = decode_receipt(&encode(&receipt).unwrap()).unwrap();
    assert_eq!(decoded.extensions["extra"], json!({"a":1}));
    assert_eq!(decoded.extensions["settlement"], receipt["settlement"]);
    let minimal = json!({"method":"tempo","reference":"ref","status":"success","timestamp":"2026-01-01T00:00:00+01:00"});
    assert!(decode_receipt(&encode(&minimal).unwrap()).is_ok());
    for (field, bad) in [
        ("method", json!("")),
        ("reference", json!(5)),
        ("timestamp", json!("bad")),
        ("status", json!("failed")),
        ("challengeId", json!("")),
        ("subscriptionId", json!("")),
        ("settlement", json!({})),
        ("settlement", json!({"amount":"1"})),
        ("externalId", json!(3)),
    ] {
        let mut value = receipt.clone();
        value[field] = bad;
        assert!(decode_receipt(&encode(&value).unwrap()).is_err(), "{field}");
    }
}

#[test]
fn literal_headers_and_upstream_signature_interoperate() {
    let literal = r#"pAyMeNt id="id", realm="shop\u0041", method="inflow", intent="charge", request="e30", description="Pay \"now\", then \\ later", opaque="e30""#;
    let parsed = parse_challenges(&[literal]).unwrap();
    assert_eq!(parsed[0].realm, "shopu0041");
    assert_eq!(
        parsed[0].description.as_deref(),
        Some("Pay \"now\", then \\ later")
    );
    let signed = PaymentChallenge::with_secret_key(
        "test-only-key-at-least-32-bytes-long",
        "shop",
        "inflow",
        "charge",
        Base64UrlJson::from_raw("e30"),
    );
    let parsed = parse_challenges(&[&render_challenge(&signed).unwrap()]).unwrap();
    assert!(parsed[0].verify("test-only-key-at-least-32-bytes-long"));
    let credential = Credential {
        challenge: parsed[0].clone(),
        payload: Default::default(),
        source: None,
    };
    let header = format!("Payment {}", encode_credential(&credential).unwrap());
    let upstream = mpp::PaymentCredential::from_header(&header).unwrap();
    assert_eq!(upstream.challenge.request.raw(), "e30");
    let mut with_all = challenge();
    with_all.description = Some("café 東京 \\ path \"q\"\t".into());
    with_all.expires = Some("2030-01-01T00:00:00Z".into());
    with_all.digest = Some("sha-256=:AA==:".into());
    with_all.opaque = Some(Base64UrlJson::from_raw("e30"));
    with_all.header = Some("Payment-Authorization".into());
    let rendered = render_challenge(&with_all).unwrap();
    assert!(rendered.contains("東京"));
    assert_eq!(
        serde_json::to_value(&parse_challenges(&[&rendered]).unwrap()[0]).unwrap(),
        serde_json::to_value(with_all).unwrap()
    );
    let basic = render_challenge(&challenge()).unwrap();
    let combined = format!("Bearer token, {basic}, Basic realm=ignored, {basic}");
    assert_eq!(
        parse_challenges(&["", ",,", "Basic realm=ignored", &combined, &basic])
            .unwrap()
            .len(),
        3
    );
    let bare = "Payment id=id, realm=shop, method=inflow, intent=charge, request=e30, unknown=ok";
    assert_eq!(parse_challenges(&[bare]).unwrap().len(), 1);
    let mixed = bare.replace("intent=charge", "intent=Charge");
    assert_eq!(
        parse_challenges(&[&mixed]).unwrap()[0].intent.as_str(),
        "Charge"
    );
}

#[test]
fn headers_reject_malformed_values_without_panicking() {
    let basic = render_challenge(&challenge()).unwrap();
    for tail in [
        ", id=other",
        ", ID=other",
        ", =bad",
        ", bad(name)=x",
        ", description=two words",
        ", description=",
        ", description=\"unterminated",
        ", description=\"x\"junk",
        ", description=\"x\"\"y\"",
        ", description=\"x\u{01}\"",
        ", header=Cookie",
    ] {
        assert!(
            parse_challenges(&[&format!("{basic}{tail}")]).is_err(),
            "{tail}"
        );
    }
    for bad in [
        "Payment",
        "Payment nope",
        "Payment id=id",
        "Payment id=\"\", realm=r, method=m, intent=i, request=e30",
    ] {
        assert!(parse_challenges(&[bad]).is_err(), "{bad}");
    }
    for bad in ["\r", "\n", "\u{7f}"] {
        let mut c = challenge();
        c.realm = bad.into();
        assert!(render_challenge(&c).is_err());
        assert!(parse_challenges(&[&format!("{basic}, description=\"{bad}\"")]).is_err());
    }
    let mut c = challenge();
    c.id.clear();
    assert!(render_challenge(&c).is_err());
    // The empty input contains no Payment challenge, rather than a synthetic one.
    assert!(parse_challenges(&[]).unwrap().is_empty());
    let _: Value = decode("e30=").unwrap();
}
