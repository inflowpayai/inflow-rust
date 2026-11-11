use inflow_x402::{INFLOW_TRANSACTION, SettleResponse, decode, encode, inflow_transaction_id};
use serde_json::{Value, json};

const ID: &str = "abcdefab-1234-4567-89ab-abcdef123456";

#[test]
fn transaction_id_preserves_the_settlement_and_other_extensions() {
    for id in [ID.to_owned(), ID.to_uppercase()] {
        for schema in [
            None,
            Some(
                json!({"type":"object","properties":{"transactionId":{"type":"string","format":"uuid"}},"required":["transactionId"]}),
            ),
        ] {
            let mut extension = json!({"info":{"transactionId":id}});
            if let Some(schema) = schema {
                extension["schema"] = schema;
            }
            let wire = json!({"success":true,"transaction":"chain-reference","network":"eip155:8453",
                "extensions":{INFLOW_TRANSACTION:extension,"custom":{"values":[1,null]}}});
            let response = SettleResponse(wire.clone());
            assert_eq!(inflow_transaction_id(&response), Some(id.as_str()));
            let decoded: SettleResponse =
                serde_json::from_value(decode(&encode(&response.0)).unwrap()).unwrap();
            assert_eq!(inflow_transaction_id(&decoded), Some(id.as_str()));
            assert_eq!(response.0, wire);
            assert_eq!(decoded.0, wire);
            let mut failed = response.clone();
            failed.0["success"] = json!(false);
            assert_eq!(inflow_transaction_id(&failed), None);
        }
    }
}

#[test]
fn malformed_or_absent_metadata_does_not_become_a_transaction_id() {
    for wire in [
        Value::Null,
        json!([]),
        json!({}),
        json!({"success":"true"}),
        json!({"success":true,"transaction":ID}),
        json!({"success":true,"extensions":null}),
        json!({"success":true,"extensions":[]}),
    ] {
        assert_eq!(inflow_transaction_id(&SettleResponse(wire)), None);
    }
    for extension in [
        Value::Null,
        json!(true),
        json!(42),
        json!("bad"),
        json!([]),
        json!({}),
        json!({"info":[]}),
        json!({"info":null}),
        json!({"info":{}}),
    ] {
        let response =
            SettleResponse(json!({"success":true,"extensions":{INFLOW_TRANSACTION:extension}}));
        assert_eq!(inflow_transaction_id(&response), None);
    }
    for id in [
        Value::Null,
        json!(true),
        json!(42),
        json!([]),
        json!({}),
        json!(""),
        json!("not-a-uuid"),
        json!("x".repeat(36)),
        json!("a".repeat(36)),
        json!(format!("{ID}\n")),
        json!("x".repeat(20000)),
        json!("é".repeat(18)),
    ] {
        let response = SettleResponse(json!({"success":true,"transaction":ID,
            "extensions":{INFLOW_TRANSACTION:{"info":{"transactionId":id}}}}));
        let before = response.0.clone();
        assert_eq!(inflow_transaction_id(&response), None);
        assert_eq!(response.0, before);
    }
}
