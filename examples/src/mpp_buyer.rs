use crate::{Result, finish, http_client, send};
use inflow_mpp_buyer::{Buyer, ClientOptions, PaymentOptions, WaitOptions};
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
        return finish(response, "payment-receipt", false, out).await;
    }
    let headers = response
        .headers()
        .get_all("www-authenticate")
        .iter()
        .map(|value| value.to_str())
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let challenges = inflow_mpp::parse_challenges(&headers)?;
    // This charge example deliberately does not enroll the user in a subscription.
    let challenge = challenges
        .iter()
        .find(|challenge| {
            challenge.method.as_str() == "inflow" && challenge.intent.as_str() == "charge"
        })
        .ok_or("No InFlow charge offer; no payment was created.")?;
    writeln!(
        out,
        "Selected InFlow charge: {}",
        inflow_mpp::decode(challenge.request.raw())?
    )?;
    drop(response);

    let buyer = Buyer::new(options)?;
    let payment = buyer
        .prepare(challenge, PaymentOptions::default(), token)
        .await?;
    writeln!(
        out,
        "Approval: {}. Approve in Sandbox if requested; Ctrl-C cancels waiting.",
        payment.approval_id().unwrap_or("not required")
    )?;
    out.flush()?;
    let credential = payment.wait(WaitOptions::default()).await?;
    // The complete credential retains the signed challenge. Never print it or convert to a lossy upstream type.
    let header = credential
        .challenge
        .header
        .as_deref()
        .unwrap_or("Authorization");
    let mut value = http::HeaderValue::from_str(&format!(
        "Payment {}",
        inflow_mpp::encode_credential(&credential)?
    ))?;
    value.set_sensitive(true);
    let response = send(http.get(url).header(header, value), token).await?;
    finish(response, "payment-receipt", true, out).await
}
