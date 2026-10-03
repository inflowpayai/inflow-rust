use inflow_examples::{Result, mpp_seller, required, sandbox};
use tokio_util::sync::CancellationToken;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    inflow_examples::outcome(run().await)
}

async fn run() -> Result<()> {
    let options = sandbox(required("INFLOW_API_KEY")?);
    let secret = required("MPP_SECRET_KEY")?;
    let app = mpp_seller::router(options, secret, &CancellationToken::new()).await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
    println!("MPP Seller: http://127.0.0.1:3000/api/widgets (0.01 USDC); /free is unpaid.");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
