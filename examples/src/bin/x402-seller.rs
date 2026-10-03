use inflow_examples::{Result, required, sandbox, x402_seller};
use tokio_util::sync::CancellationToken;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    inflow_examples::outcome(run().await)
}

async fn run() -> Result<()> {
    let options = sandbox(required("INFLOW_API_KEY")?);
    let app = x402_seller::router(
        options,
        "http://127.0.0.1:3001/api/widgets",
        &CancellationToken::new(),
    )
    .await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3001").await?;
    println!("x402 Seller: http://127.0.0.1:3001/api/widgets (0.01 USDC); /free is unpaid.");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
