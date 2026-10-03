use inflow_examples::{Result, interruptible, mpp_buyer, required, sandbox, target};
use tokio_util::sync::CancellationToken;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    inflow_examples::outcome(run().await)
}

async fn run() -> Result<()> {
    let options = sandbox(required("INFLOW_API_KEY")?);
    let url = target("http://127.0.0.1:3000/api/widgets")?;
    let token = CancellationToken::new();
    interruptible(
        &token,
        mpp_buyer::run(options, url, &token, &mut std::io::stdout()),
        tokio::signal::ctrl_c(),
    )
    .await
}
