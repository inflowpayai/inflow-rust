use inflow_examples::{Result, required, sandbox};
use inflow_mpp_buyer::{Buyer, CancellationToken, PaymentStatusOptions};

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    inflow_examples::outcome(run().await)
}

async fn run() -> Result<()> {
    let buyer = Buyer::new(sandbox(required("INFLOW_API_KEY")?))?;
    // The same transaction endpoint reports both MPP and x402 settlement; this never creates a payment.
    let status = buyer
        .get_payment_status(
            &required("TRANSACTION_ID")?,
            PaymentStatusOptions::default(),
            &CancellationToken::new(),
        )
        .await?;
    println!(
        "Transaction: {}\nStatus: {}",
        status["transactionId"], status["status"]
    );
    if let Some(action) = status.get("nextAction") {
        println!(
            "Buyer action: {}\nDashboard: {}",
            action["type"], action["url"]
        );
    }
    Ok(())
}
