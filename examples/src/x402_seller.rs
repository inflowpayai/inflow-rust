use crate::Result;
use axum::{Json, Router, routing::get};
use inflow_x402_seller::{ClientOptions, OfferOptions, Seller};
use serde_json::json;
use tokio_util::sync::CancellationToken;

pub async fn router(
    options: ClientOptions,
    resource: &str,
    token: &CancellationToken,
) -> Result<Router> {
    let seller = Seller::new(options, token).await?;
    let route = seller.route(&OfferOptions::new("0.01 USDC"), token).await?;
    let layer = inflow_x402_axum::payment_layer(seller.facilitator(), route, resource)?;
    // Upstream verifies, runs the handler, and settles before releasing the response.
    // Keep irreversible application side effects out of this handler.
    Ok(Router::new()
        .route("/free", get(|| async { Json(json!({"ok":true})) }))
        .route(
            "/api/widgets",
            get(|| async { Json(json!({"widgets":[1,2,3]})) }).route_layer(layer),
        ))
}
