use crate::{Result, finish, http_client, send};
use inflow_x402_buyer::{
    Buyer, BuyerOptions, ClientOptions, OriginalJson, PaymentRequired, SignOptions, WaitOptions,
};
use std::io::Write;
use tokio_util::sync::CancellationToken;

pub async fn run(
    options: ClientOptions,
    url: url::Url,
    token: &CancellationToken,
    out: &mut dyn Write,
) -> Result<()> {
    let http = http_client()?;
    let response = send(http.get(url.clone()), token).await?;
    if response.status() != 402 {
        return finish(response, "payment-response", false, out).await;
    }
    let header = response
        .headers()
        .get("payment-required")
        .ok_or("402 response has no Payment-Required header.")?
        .to_str()?;
    let required: PaymentRequired<OriginalJson> =
        serde_json::from_value(inflow_x402::decode(header)?)?;
    drop(response);
    let buyer = Buyer::new(
        BuyerOptions {
            client: options,
            ..Default::default()
        },
        token,
    )
    .await?;
    let selected = buyer
        .select(&required, token)
        .await?
        .ok_or("No supported InFlow offer; no payment was created.")?;
    writeln!(
        out,
        "Selected payment terms: {}",
        serde_json::to_string(&selected)?
    )?;
    let payment = buyer
        .prepare(&selected, &required, SignOptions::default(), token)
        .await?;
    writeln!(
        out,
        "Approval: {}. Approve in Sandbox if requested; Ctrl-C cancels waiting.",
        payment.approval_id()
    )?;
    out.flush()?;
    let signed = payment.wait(WaitOptions::default()).await?;
    let mut header = http::HeaderValue::from_str(&signed.encoded_payload)?;
    header.set_sensitive(true);
    // Reuse the exact destination once. Neither a redirect nor another 402 starts a second payment.
    let response = send(http.get(url).header("Payment-Signature", header), token).await?;
    finish(response, "payment-response", true, out).await
}
