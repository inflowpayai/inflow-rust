use inflow_examples::{Result, mpp_seller, required, sandbox};
use tokio_util::sync::CancellationToken;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    inflow_examples::outcome(run().await)
}

async fn run() -> Result<()> {
    let options = sandbox(required("INFLOW_API_KEY")?);
    let secret = required("MPP_SECRET_KEY")?;
    let token = CancellationToken::new();
    let method = std::env::var("MPP_METHOD").unwrap_or_else(|_| "inflow".into());
    let app = match method.as_str() {
        "inflow" => mpp_seller::router(options, secret, &token).await?,
        "card" | "stripe" => {
            let selected = if method == "card" {
                inflow_mpp_seller::Method::Card
            } else {
                inflow_mpp_seller::Method::Stripe
            };
            mpp_seller::router_with_method(
                options,
                secret,
                selected,
                serde_json::json!({"amount":"1.25","externalId":"widgets"}),
                &token,
            ).await?
        }
        "instrument" => mpp_seller::router_with_method(
            options,
            secret,
            inflow_mpp_seller::Method::Inflow,
            serde_json::json!({"amount":"1.25","currency":"USD","methodDetails":{"rail":"instrument"}}),
            &token,
        ).await?,
        _ => return Err("MPP_METHOD must be inflow, instrument, card, or stripe.".into()),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
    println!(
        "MPP Seller: http://127.0.0.1:3000/api/widgets; /free is unpaid. Method: {method}; price: {}.",
        if method == "inflow" {
            "0.01 USDC"
        } else {
            "1.25 USD"
        }
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
