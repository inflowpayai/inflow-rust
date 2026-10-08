mod mpp;
mod runtime;
mod tap;
mod transport;
mod x402;

use inflow_core::Error;
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};

fn bad(message: &str) -> Error {
    Error::new("ADAPTER_ERROR", message)
}
fn string<'a>(value: &'a Value, name: &str) -> Result<&'a str, Error> {
    value[name]
        .as_str()
        .ok_or_else(|| bad("missing string input"))
}
fn read<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, Error> {
    serde_json::from_value(value).map_err(|_| bad("invalid adapter input"))
}
fn wire(value: impl serde::Serialize) -> Result<Value, Error> {
    serde_json::to_value(value).map_err(|_| bad("cannot serialize SDK result"))
}

async fn respond(request: Value) -> Value {
    let mut response =
        json!({"adapter_version":"1","sequence":request["sequence"],"case_id":request["case_id"]});
    let result = async {
        if request["adapter_version"] != "1" {
            return Err(bad("unsupported adapter version"));
        }
        let op = string(&request, "operation")?;
        let input = &request["input"];
        let result = if op.starts_with("mpp.") {
            mpp::execute(op, input).await
        } else if op.starts_with("x402.") {
            x402::execute(op, input).await
        } else if op.starts_with("runtime.") {
            runtime::execute(op, input).await
        } else if op == "tap.seller.verify" {
            tap::execute(input).await
        } else {
            Err(bad("unknown operation"))
        };
        match result {
            Ok(value) => Ok(json!({"result":value})),
            Err(error) => Ok(json!({"error":classify(error, op, input)})),
        }
    }
    .await;
    let observation = result
        .unwrap_or_else(|error| json!({"error":{"code":"ADAPTER_ERROR","message":error.message}}));
    response
        .as_object_mut()
        .expect("object envelope")
        .extend(observation.as_object().expect("object observation").clone());
    response
}

fn classify(error: Error, op: &str, input: &Value) -> Value {
    let mut details = json!({});
    let (code, message) = match error.code.as_str() {
        "MPP_PAYMENT_FAILED" => {
            let problem = if op.starts_with("mpp.buyer.") {
                if let Some(id) = error.body["transactionId"].as_str() {
                    details["transaction_id"] = json!(id);
                }
                &error.body["problem"]
            } else {
                &error.body
            };
            if input["include_problem"] != false && !problem.is_null() {
                details["problem"] = problem.clone();
            }
            ("payment-failed", "Payment failed.")
        }
        "MPP_PAYMENT_TIMEOUT" | "MPP_PAYMENT_EXPIRED" => {
            if let Some(id) = error.body["transactionId"].as_str() {
                details["transaction_id"] = json!(id);
            }
            if error.code == "MPP_PAYMENT_TIMEOUT" {
                ("payment-timeout", "Payment timed out.")
            } else {
                ("payment-expired", "Payment expired.")
            }
        }
        "MPP_PAYMENT_CANCELLED" | "X402_APPROVAL_CANCELLED" => {
            ("payment-cancelled", "Payment cancelled.")
        }
        "MPP_MALFORMED_CREDENTIAL" | "MPP_CREDENTIAL_MISMATCH" => {
            ("invalid-credential", "Invalid credential.")
        }
        "INVALID_MPP_DATA" => {
            if op == "mpp.core.decode-credential" {
                ("invalid-credential", "Invalid credential.")
            } else {
                ("invalid-input", "Invalid input.")
            }
        }
        "MPP_CARD_UNAVAILABLE"
        | "MPP_STRIPE_UNAVAILABLE"
        | "MPP_UNSUPPORTED_CURRENCY"
        | "MPP_AMBIGUOUS_RAIL"
        | "MPP_UNSUPPORTED_RAIL"
        | "MPP_INSTRUMENT_REQUIRED"
        | "X402_ADAPTER_ROUTING_ERROR" => {
            ("unsupported-capability", "Unsupported payment capability.")
        }
        "X402_APPROVAL_FAILED" => {
            details["status"] = error.body["status"].clone();
            ("payment-failed", "Payment failed.")
        }
        "X402_APPROVAL_TIMEOUT" => ("payment-timeout", "Payment timed out."),
        "X402_PAYMENT_ID_FORMAT" => ("invalid-input", "Invalid input."),
        "INVALID_X402_CONFIGURATION"
            if matches!(op, "x402.seller.offers" | "x402.seller.route")
                && matches!(
                    error.message.as_str(),
                    "price must not contain leading or trailing whitespace"
                        | "invalid price; use $0.01, 0.01 USDC, or an explicit currency"
                        | "price must be a nonnegative decimal with at most eight decimal places"
                        | "price requires a currency"
                        | "price cannot be represented without truncation"
                        | "instrument payments require USD 0.50–92233720368547758.07 in whole cents"
                ) =>
        {
            ("invalid-input", "Invalid input.")
        }
        _ if (op.starts_with("x402.") || op == "mpp.buyer.payment-status")
            && error.http_status > 0 =>
        {
            return json!({"code":"api-error","message":"InFlow API request failed.","http_status":error.http_status,"details":{"body":*error.body}});
        }
        _ => {
            return json!({"code":"ADAPTER_ERROR","message":format!("Unexpected SDK error {}: {}",error.code,error.message)});
        }
    };
    let mut value = json!({"code":code,"message":message});
    if details != json!({}) {
        value["details"] = details;
    }
    value
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).is_some_and(|v| v == "--sign-challenge") {
        println!(
            "{}",
            mpp::sign(serde_json::from_str(
                args.get(2).ok_or("missing challenge")?
            )?)?
        );
    } else if args.get(1).is_some_and(|v| v == "--adapter") {
        for line in io::stdin().lock().lines() {
            let line = line?;
            if line.len() > 1024 * 1024 {
                return Err("oversized adapter input".into());
            }
            println!("{}", respond(serde_json::from_str(&line)?).await);
            io::stdout().flush()?;
        }
    }
    Ok(())
}
