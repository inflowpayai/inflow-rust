pub mod mpp_buyer;
pub mod mpp_seller;
pub mod x402_buyer;
pub mod x402_seller;

use inflow_core::{Authentication, ClientOptions, Environment};
use std::{future::Future, io::Write, time::Duration};
use tokio_util::sync::CancellationToken;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub fn outcome(result: Result<()>) -> std::process::ExitCode {
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            // Display the explanation, not Debug's complete error body or headers.
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}

pub fn required(name: &str) -> Result<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("Set {name}; see examples/README.md.").into())
}

pub fn sandbox(key: String) -> ClientOptions {
    ClientOptions {
        environment: Environment::Sandbox,
        authentication: Authentication::ApiKey(key),
        ..Default::default()
    }
}

pub fn target(default: &str) -> Result<url::Url> {
    merchant_url(&std::env::var("TARGET_URL").unwrap_or_else(|_| default.into()))
}

pub fn merchant_url(value: &str) -> Result<url::Url> {
    let url = url::Url::parse(value)?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("TARGET_URL must be HTTP or HTTPS without embedded credentials.".into());
    }
    Ok(url)
}

pub fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(Duration::from_secs(30))
        .build()?)
}

pub async fn interruptible<T>(
    token: &CancellationToken,
    operation: impl Future<Output = Result<T>>,
    signal: impl Future<Output = std::io::Result<()>>,
) -> Result<T> {
    tokio::pin!(operation);
    tokio::select! {
        result = &mut operation => result,
        result = signal => {
            result?;
            token.cancel();
            // Keep polling: the SDK's wait future must finish its approval cleanup before exit.
            operation.await
        }
    }
}

pub async fn send(
    request: reqwest::RequestBuilder,
    token: &CancellationToken,
) -> Result<reqwest::Response> {
    tokio::select! {
        biased;
        _ = token.cancelled() => Err("Request cancelled; cancellation is not a payment reversal.".into()),
        response = request.send() => Ok(response?),
    }
}

pub async fn finish(
    mut response: reqwest::Response,
    receipt_header: &str,
    paid: bool,
    out: &mut dyn Write,
) -> Result<()> {
    let status = response.status();
    let receipt = response.headers().get(receipt_header).cloned();
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if chunk.len() > 1024 * 1024 - body.len() {
            return Err("Resource body exceeds this example's 1 MiB limit.".into());
        }
        body.extend_from_slice(&chunk);
    }
    writeln!(out, "HTTP {status}\n{}", String::from_utf8_lossy(&body))?;
    if !status.is_success() {
        return Err("Resource request failed. Check transactions before retrying; no second payment was attempted.".into());
    }
    match receipt {
        Some(header) if receipt_header == "payment-receipt" => {
            let receipt = inflow_mpp::decode_receipt(header.to_str()?)?;
            writeln!(out, "Receipt: {} / {}", receipt.method, receipt.reference)?;
        }
        Some(header) => {
            let receipt = inflow_x402::decode(header.to_str()?)?;
            if receipt["success"] != true {
                return Err("Settlement receipt does not report success. Check transactions before retrying.".into());
            }
            writeln!(
                out,
                "Receipt: {} / {}",
                receipt["network"], receipt["transaction"]
            )?;
        }
        None if paid => {
            return Err("Paid response has no receipt. Check transactions before retrying.".into());
        }
        None => writeln!(out, "No payment was initiated by this program.")?,
    }
    Ok(())
}
