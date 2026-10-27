use crate::Result;
use axum::{Json, Router, routing::get};
use inflow_x402_seller::{ClientOptions, OfferOptions, PaymentFlow, Seller};
use serde_json::json;
use tokio_util::sync::CancellationToken;

pub async fn router(
    options: ClientOptions,
    resource: &str,
    token: &CancellationToken,
    payment_flow: PaymentFlow,
) -> Result<Router> {
    let seller = Seller::new(options, token).await?;
    let mut offers = OfferOptions::new("0.01 USDC");
    offers.payment_flow = payment_flow;
    let route = seller.route(&offers, token).await?;
    let layer = inflow_x402_axum::payment_layer(seller.facilitator(), route, resource)?;
    // Authorization runs this handler before settlement; upfront charges before it runs.
    // A failed handler after upfront settlement needs application reconciliation, not another payment.
    Ok(Router::new()
        .route("/free", get(|| async { Json(json!({"ok":true})) }))
        .route(
            "/api/widgets",
            get(|| async { Json(json!({"widgets":[1,2,3]})) }).route_layer(layer),
        ))
}
