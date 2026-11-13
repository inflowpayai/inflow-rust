use crate::Result;
use axum::{
    Json, Router,
    http::{Uri, request::Parts},
    routing::get,
};
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
    let mut baseline = OfferOptions::new("0.01 USDC");
    baseline.payment_flow = payment_flow;
    inflow_x402_axum::payment_layer(
        seller.facilitator(),
        seller.route(&baseline, token).await?,
        resource,
    )?;
    let resource = resource.to_owned();
    let layer = inflow_x402_axum::RequestPaymentLayer::new(move |parts: Parts, _| {
        let seller = seller.clone();
        let resource = resource.clone();
        async move {
            let mut offers = OfferOptions::new(if large_bundle(&parts.uri) {
                "0.02 USDC"
            } else {
                "0.01 USDC"
            });
            offers.payment_flow = payment_flow;
            let route = seller.route(&offers, &CancellationToken::new()).await?;
            inflow_x402_axum::payment_layer(seller.facilitator(), route, &resource)
        }
    });
    // Authorization runs this handler before settlement; upfront charges before it runs.
    // A failed handler after upfront settlement needs application reconciliation, not another payment.
    Ok(Router::new()
        .route("/free", get(|| async { Json(json!({"ok":true})) }))
        .route(
            "/api/widgets",
            get(|uri: Uri| async move { Json(json!({"widgets": if large_bundle(&uri) { vec![1,2,3,4,5,6] } else { vec![1,2,3] }})) }).route_layer(layer),
        ))
}

fn large_bundle(uri: &Uri) -> bool {
    url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
        .any(|(key, value)| key == "bundle" && value == "large")
}
