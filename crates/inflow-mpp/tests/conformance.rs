use inflow_mpp::*;
use serde_json::Value;

// Generated mpp-core cases from inflow-specs 15d98fae5a8abe321036074bdd1afaac67b56aeb.
#[test]
fn shared_core_cases_through_public_codecs() {
    let cases: Vec<Value> = serde_json::from_str(include_str!("data/core.json")).unwrap();
    assert_eq!(cases.len(), 18);
    for case in cases {
        let input = &case["input"];
        let encoded = input["value"].as_str().unwrap_or_default();
        let actual = match case["operation"].as_str().unwrap() {
            "mpp.core.encode" => encode(&input["value"]).map(Value::String),
            "mpp.core.decode" => decode(encoded),
            "mpp.core.decode-credential" => {
                decode_credential(encoded).map(|v| serde_json::to_value(v).unwrap())
            }
            "mpp.core.decode-receipt" => {
                decode_receipt(encoded).map(|v| serde_json::to_value(v).unwrap())
            }
            "mpp.core.parse-challenges" => {
                let headers = match &input["headers"] {
                    Value::Array(values) => values.iter().map(|v| v.as_str().unwrap()).collect(),
                    Value::String(value) => vec![value.as_str()],
                    _ => panic!("invalid fixture"),
                };
                parse_challenges(&headers).map(|v| serde_json::to_value(v).unwrap())
            }
            other => panic!("unimplemented operation: {other}"),
        };
        if let Some(expected) = case["expect"].get("result") {
            assert_eq!(
                &actual.unwrap_or_else(|error| panic!("{}: {error}", case["id"])),
                expected,
                "{}",
                case["id"]
            );
        } else {
            assert_eq!(
                actual.unwrap_err().code,
                "INVALID_MPP_DATA",
                "{}",
                case["id"]
            );
        }
    }
}
