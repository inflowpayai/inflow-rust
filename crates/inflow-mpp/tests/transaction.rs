use inflow_mpp::{Receipt, decode, decode_receipt, encode, inflow_transaction_id};
use serde_json::{Value, json};

const ID: &str = "abcdefab-1234-4567-89ab-abcdef123456";

#[test]
fn transaction_id_survives_upstream_receipt_headers_without_changing_references() {
    for id in [ID.to_owned(), ID.to_uppercase()] {
        let wire = json!({"method":"inflow","status":"success","reference":"payment-reference",
            "timestamp":"2026-09-01T12:00:00.123456789Z","inflowTransactionId":id,
            "externalId":"order","subscriptionId":"subscription","challengeId":"challenge",
            "settlement":{"amount":"1.25","currency":"USDC"},"custom":{"values":[1,null]}});
        let receipt = decode_receipt(&encode(&wire).unwrap()).unwrap();
        assert_eq!(inflow_transaction_id(&receipt), Some(id.as_str()));
        let header = receipt.to_header().unwrap();
        assert_eq!(decode(&header).unwrap(), wire);
        let upstream = Receipt::from_header(&header).unwrap();
        assert_eq!(inflow_transaction_id(&upstream), Some(id.as_str()));
        assert_eq!(upstream.reference, "payment-reference");
        assert_eq!(serde_json::to_value(&receipt).unwrap(), wire);
    }
}

#[test]
fn absent_or_malformed_transaction_ids_are_not_payment_references() {
    let mut receipt = Receipt::success("inflow", ID);
    receipt.external_id = Some(ID.into());
    receipt.subscription_id = Some(ID.into());
    assert_eq!(inflow_transaction_id(&receipt), None);
    for value in [
        Value::Null,
        json!(true),
        json!(42),
        json!([]),
        json!({}),
        json!(""),
        json!("not-a-uuid"),
        json!("x".repeat(36)),
        json!("a".repeat(36)),
        json!(ID.replace('-', "a")),
        json!(ID.replace('a', "-")),
        json!(format!("{ID}\n")),
        json!("x".repeat(20000)),
        json!("é".repeat(18)),
    ] {
        receipt
            .extensions
            .insert("inflowTransactionId".into(), value);
        let before = serde_json::to_value(&receipt).unwrap();
        assert_eq!(inflow_transaction_id(&receipt), None);
        assert_eq!(serde_json::to_value(&receipt).unwrap(), before);
        let decoded = decode_receipt(&receipt.to_header().unwrap()).unwrap();
        assert_eq!(inflow_transaction_id(&decoded), None);
        assert_eq!(decoded.reference, ID);
    }
}
