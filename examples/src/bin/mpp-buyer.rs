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
    let card = match std::env::var("MPP_METHOD").as_deref() {
        Ok("card") => Some(inflow_mpp_buyer::CardPaymentOptions {
            merchant: inflow_mpp_buyer::Merchant {
                name: required("MERCHANT_NAME")?,
                url: required("MERCHANT_URL")?,
                country_code: required("MERCHANT_COUNTRY")?,
            },
            instrument_id: std::env::var("INSTRUMENT_ID").ok(),
        }),
        Ok("stripe") => {
            return Err(
                "Stripe tokens must be supplied by an external Stripe-capable payer.".into(),
            );
        }
        Ok("inflow" | "instrument") | Err(_) => None,
        _ => return Err("MPP_METHOD must be inflow, instrument, or card.".into()),
    };
    interruptible(
        &token,
        mpp_buyer::run_with_card(options, url, card, &token, &mut std::io::stdout()),
        tokio::signal::ctrl_c(),
    )
    .await
}
