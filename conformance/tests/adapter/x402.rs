use crate::{bad, read, string, transport, wire};
use inflow_core::Error;
use serde_json::{Value, json};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub async fn execute(op: &str, input: &Value) -> Result<Value, Error> {
    match op {
        "x402.core.identifier-valid" => {
            return Ok(json!(
                input["value"]
                    .as_str()
                    .is_some_and(inflow_x402::valid_payment_id)
            ));
        }
        "x402.core.identifier-declaration" => return Ok(inflow_x402::identifier_declaration()),
        "x402.core.identifier-entry" => {
            return Ok(inflow_x402::identifier_entry(
                &input["declaration"],
                string(input, "payment_id")?,
            )
            .unwrap_or(Value::Null));
        }
        _ => {}
    }
    let token = CancellationToken::new();
    if matches!(op, "x402.buyer.sign" | "x402.buyer.cancel") {
        use inflow_x402_buyer::{Buyer, BuyerOptions, SignOptions, WaitOptions};
        let buyer = Buyer::new(
            BuyerOptions {
                client: transport::options(input)?,
                instrument_id: input["instrument_id"].as_str().map(str::to_owned),
                ..Default::default()
            },
            &token,
        )
        .await?;
        let requirement = read(input["requirement"].clone())?;
        let mut context = input["context"].clone();
        context["accepts"] = json!([input["requirement"]]);
        let payment = buyer
            .prepare(
                &requirement,
                &read(context)?,
                SignOptions {
                    payment_id: input["payment_id"].as_str().map(str::to_owned),
                    ..Default::default()
                },
                &token,
            )
            .await?;
        if op == "x402.buyer.cancel" {
            let _ = payment.cancel().await;
        }
        let value = payment
            .wait(WaitOptions {
                poll_interval: Duration::from_millis(
                    input["poll_interval_ms"].as_u64().unwrap_or(1),
                ),
                timeout: Duration::from_millis(input["timeout_ms"].as_u64().unwrap_or(2000)),
            })
            .await?;
        return Ok(
            json!({"encodedPayload":value.encoded_payload,"paymentPayload":value.payment_payload,"transactionId":value.transaction_id}),
        );
    }
    if matches!(op, "x402.seller.offers" | "x402.seller.route") {
        use inflow_x402_seller::{OfferOptions, Price, Seller};
        let seller = Seller::new(transport::options(input)?, &token).await?;
        let source = &input["options"];
        let mut options = OfferOptions::new(Price {
            amount: string(source, "price")?.into(),
            currency: source["currency"].as_str().map(str::to_owned),
        });
        if let Some(value) = source["maxTimeoutSeconds"].as_u64() {
            options.max_timeout_seconds = value;
        }
        options.schemes = source.get("schemes").cloned().map(read).transpose()?;
        options.networks = source.get("networks").cloned().map(read).transpose()?;
        options.permit2 = source["assetTransferMethod"] == "permit2";
        let (accepts, extensions) = if op == "x402.seller.route" {
            let route = seller.route(&options, &token).await?;
            (route.accepts, route.extensions)
        } else {
            (seller.offers(&options, &token).await?, Default::default())
        };
        let accepts: Vec<_>=accepts.into_iter().map(|v|json!({"scheme":v.scheme,"network":v.network,"payTo":v.pay_to,"price":{"asset":v.asset,"amount":v.amount},"maxTimeoutSeconds":v.max_timeout_seconds,"extra":v.extra})).collect();
        if op == "x402.seller.offers" {
            return Ok(json!(accepts));
        }
        let mut value = json!({"accepts":accepts});
        if !extensions.is_empty() {
            value["extensions"] = json!(extensions);
        }
        return Ok(value);
    }
    use inflow_x402_seller::Facilitator;
    let facilitator = Facilitator::new(transport::options(input)?)?;
    let request = inflow_x402::facilitator_request(
        &input["payment_payload"],
        &input["payment_requirements"],
    )?;
    match op {
        "x402.seller.verify" => wire(facilitator.verify(&request, &token).await?),
        "x402.seller.settle" => wire(facilitator.settle(&request, &token).await?),
        "x402.seller.verify-settle" => {
            let verification = wire(facilitator.verify(&request, &token).await?)?;
            if verification["isValid"] == true {
                Ok(
                    json!({"verification":verification,"settlement":wire(facilitator.settle(&request,&token).await?)?}),
                )
            } else {
                Ok(json!({"verification":verification}))
            }
        }
        _ => Err(bad("unknown x402 operation")),
    }
}
