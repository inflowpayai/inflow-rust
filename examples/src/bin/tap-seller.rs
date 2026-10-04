use inflow_examples::{Result, required, tap_seller};
use inflow_tap_seller::Verifier;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    inflow_examples::outcome(run().await)
}
async fn run() -> Result<()> {
    let origin = required("PUBLIC_ORIGIN")?;
    // One verifier preserves the trusted-key cache and nonce history across requests.
    let app = tap_seller::router(Verifier::new(Default::default())?, &origin)?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3002").await?;
    println!("TAP Seller: http://127.0.0.1:3002/api/catalog; signed origin: {origin}");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
