# inflow-mpp-buyer

Obtain InFlow payment credentials for MPP challenges. The Buyer creates a payment
once, waits for approval when required, and returns the platform's credential.
It supports InFlow charge, InFlow subscription, and Tempo charge challenges.
It does not send the credential to the selling Service: your HTTP or MCP client
owns that step.

[Source](https://github.com/inflowpayai/inflow-rust/tree/main/crates/inflow-mpp-buyer) ·
[MPP](https://mpp.dev/) · [Core codecs](../inflow-mpp/README.md)

## Account and client

For a runnable Buyer and matching Seller, follow the
[Sandbox example walkthrough](https://github.com/inflowpayai/inflow-rust/tree/main/examples#mpp).

Use an InFlow account permitted to buy, with its API key or OAuth access token.
A Seller account can also act as a buyer; it is not necessary to create a second
account just because the account is a Seller.
[Sandbox](https://sandbox.inflowpay.ai) and [production](https://app.inflowpay.ai)
have separate credentials. Set `Environment::Sandbox` for testing; the default
is production. `Authentication::Bearer` accepts an `AccessTokenProvider` from
`inflow-core`. Anonymous configuration sends no authentication and does not bypass
the platform's account requirements.

Use the application's Tokio runtime. Construction makes no network calls.
`Buyer` can be cloned and used for concurrent payments; each payment has its own
cancellation token and pending budget.

## Obtain a credential

```no_run
use inflow_mpp_buyer::{
    Authentication, Buyer, CancellationToken, ClientOptions, Environment,
    PaymentChallenge, PaymentOptions, WaitOptions, Credential,
};

async fn approve(challenge: &PaymentChallenge) -> Result<Credential, Box<dyn std::error::Error>> {
    let buyer = Buyer::new(ClientOptions {
        environment: Environment::Sandbox,
        authentication: Authentication::ApiKey(std::env::var("INFLOW_API_KEY")?),
        ..Default::default()
    })?;
    let cancellation = CancellationToken::new();
    let payment = buyer.prepare(challenge, PaymentOptions::default(), &cancellation).await?;
    // The application can display these identifiers while approval is pending.
    let approval_id = payment.approval_id();
    let transaction_id = payment.transaction_id();
    let credential = payment.wait(WaitOptions::default()).await?;
    Ok(credential)
}
```

Pass a challenge parsed with `inflow_mpp_buyer::parse_challenges`. Inspect its
decoded request and choose a method, currency, and amount appropriate for your
application **before** calling `prepare`; the Buyer does not choose a spending
policy. The platform can return a credential immediately or require approval.
This is not a local-wallet signer.

`PaymentOptions::instrument_id` selects an instrument for an InFlow charge.
It does not select the rail: the seller's challenge specifies the rail.
For an InFlow subscription, omit `subscription_id` to request initial approval,
or supply an existing subscription identifier to request a credential for the
current challenge. That authorization does not create a new subscription.
Tempo charges use the default empty options.

## Send the credential using HTTP or MCP

For HTTP:

1. Send your resource request with your own HTTP client. Disable automatic
   redirects when replaying payment credentials.
2. On a 402 response, collect every `WWW-Authenticate` value and pass them to
   `parse_challenges`. Choose an offer explicitly and call `prepare` and `wait`.
3. Encode the returned credential with `encode_credential`. Send `Payment `
   followed by that encoded value in the challenge's `header` field when present,
   or `Authorization` otherwise. Preserve any separate Service authentication.
4. Read the resource response and, when present, its `Payment-Receipt` with
   `inflow_mpp::decode_receipt`. A credential is not a receipt or proof of delivery.

```rust
use inflow_mpp_buyer::{Credential, Error, encode_credential};

fn payment_header(credential: &Credential) -> Result<(String, String), Error> {
    let name = credential.challenge.header.as_deref().unwrap_or("Authorization").to_owned();
    let value = format!("Payment {}", encode_credential(credential)?);
    Ok((name, value))
}
```

Only replay a request whose body you can reproduce. Send the credential only to
the intended Service, and do not automatically make another payment after an
ambiguous network failure. Use a request body and destination consistent with the
chosen challenge. This crate does not buffer streams, follow redirects, or retry
resource delivery for you.

For MCP, obtain the `PaymentChallenge` from the payment-required tool response,
choose the offer, and use the same Buyer lifecycle. Your MCP client's payment
metadata handling must serialize the complete returned credential. Do not convert
it into upstream `mpp::PaymentCredential`: that type cannot retain `description`
in mpp0.14.0. No concrete MCP client dependency is required by this crate.

## Waiting and cancellation

The default pending budget is 15 minutes, starting when payment creation returns.
It includes time spent before calling `wait`, polling delays, and polling requests.
The default interval is five seconds; server `retryAfterSeconds` takes precedence.
Zero polling interval is supported. Each platform request also has the separate
`ClientOptions::timeout`. Creation and subscription authorization do not retry
automatically; a lost response could already have created an approval or credential.

- Pass a cancellation token to `prepare` to interrupt creation or authorization.
  Cancelling it also stops the resulting payment's wait.
- Before consuming a handle in `wait`, obtain `payment.cancellation()` if another
  task needs to stop that wait. This token affects only that payment.
- `payment.cancel().await` stops the operation and waits for the known approval's
  cancellation attempt. Repeated or concurrent calls share that attempt.
- `buyer.cancel_approval(id).await` cancels a known approval, including one retained
  by the application. It does not cancel a subscription or reverse a payment.

Cleanup has an independent five-second limit and no retries. Explicit cancellation
reports its failure; automatic cleanup preserves the original payment error.
Dropping a pending payment or its wait future schedules best-effort cleanup on
the application's runtime. Runtime shutdown can prevent that cleanup from running.
If creation was interrupted before an approval identifier arrived, there is no
known approval to cancel; server expiry remains the backstop.

Successful credential completion disarms approval cleanup. A subsequent failure
to deliver the credential or receive the resource is not payment reversal.

## Errors

`Error::code` distinguishes `MPP_PAYMENT_CANCELLED`, `MPP_PAYMENT_TIMEOUT`,
`MPP_PAYMENT_EXPIRED`, `MPP_PAYMENT_FAILED`, and `MPP_MALFORMED_CREDENTIAL`.
Payment failures retain the platform problem in `Error::body`; timeout and
transaction-expiry errors retain `transactionId` there when supplied. Transport
and authentication errors retain their core error codes and HTTP metadata.
Do not log credentials or payment payloads.

## Upstream automatic-client compatibility

This is an explicit credential workflow, not a `mpp::client::PaymentProvider`
implementation. The upstream automatic HTTP path parses headers before invoking
a provider and serializes its own credential type afterward. Those steps bypass
our corrected header parsing and discard the challenge description in mpp0.14.0.
The MCP provider interface uses the same credential type.

[Upstream PR #490](https://github.com/tempoxyz/mpp-rs/pull/490) addresses description
preservation; [issue #556](https://github.com/tempoxyz/mpp-rs/issues/556) tracks the
header parser. Using your own HTTP/MCP client with the complete credential avoids
both limitations without replacing the upstream automatic-payment engine.
