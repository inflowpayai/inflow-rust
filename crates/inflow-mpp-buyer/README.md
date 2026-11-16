# inflow-mpp-buyer

Obtain InFlow payment credentials for MPP challenges. The Buyer creates a payment
once, waits for approval when required, and returns the platform's credential.
It supports InFlow charge, InFlow subscription, Tempo charge, and Visa CARD charge challenges.
The manual Buyer returns a credential for your HTTP or MCP client to send.
`HttpPayment` connects that lifecycle to upstream's automatic charge-only HTTP
client.

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

## Automatic HTTP payments

Use `HttpPayment` with upstream `mpp::client::ChargeClient` to request a resource,
answer its payment challenge through InFlow, and send the resulting credential.
The upstream client owns HTTP delivery and disables redirects. InFlow supplies
credentials; it does not replace upstream's HTTP transport or retry loop.
This path also uses upstream's challenge parser; the
[quoted-string compatibility note](../inflow-mpp/README.md#differences-from-upstream-mpp)
applies to it.

This example uses `mpp` 0.20 with its `client` feature and `reqwest` 0.13 alongside
`inflow-mpp-buyer`. Setting `Accept-Payment` on the individual request opts into
payment. Without it, a 402 is returned without creating a payment.

```no_run
use inflow_mpp_buyer::{Authentication, Buyer, ClientOptions, Environment, HttpPayment, HttpPaymentMethod};
use mpp::client::ChargeClient;

async fn buy() -> Result<(), Box<dyn std::error::Error>> {
    let buyer = Buyer::new(ClientOptions {
        environment: Environment::Sandbox,
        authentication: Authentication::ApiKey(std::env::var("INFLOW_API_KEY")?),
        ..Default::default()
    })?;
    let payment = HttpPayment::new(buyer, HttpPaymentMethod::Inflow { instrument_id: None });
    let client = ChargeClient::new(reqwest::Client::builder())?;
    let response = payment.send(
        &client,
        client.request(reqwest::Method::GET, "https://seller.example/resource")
            .header("Accept-Payment", "inflow/charge"),
    ).await?;
    // A resource failure after payment does not imply a refund or authorize another payment.
    response.error_for_status()?;
    Ok(())
}
```

Choose `HttpPaymentMethod::Tempo` for Tempo charges or `HttpPaymentMethod::Card`
with `CardPaymentOptions` for Visa CARD charges. Subscriptions use the manual
Buyer API: upstream's `ChargeClient` handles charge intents only. Challenge
preferences and exclusions follow the request's `Accept-Payment` header.
For challenges selecting `Payment-Authorization`, the credential uses that header
and a separate application `Authorization` header is preserved. InFlow's platform
credentials are not sent to the selling Service.

The payment method is fixed at construction. Clone the payment configuration and
use `with_instrument_id`, `with_wait_options`, or `with_cancellation` for request-specific
settings. `with_instrument_id(None)` clears an instrument; Tempo rejects instrument
overrides. These builders do not mutate other clones or replace their limits.
Give independent requests independent cancellation tokens; supplying
the same token deliberately groups their cancellation. Cancellation stops
credential creation and approval waiting, not HTTP resource delivery. The existing
bounded approval cleanup applies when pending payment work is cancelled or dropped.

### Optional payment limits

`HttpPayment::new_with_limits` accepts `MethodLimits` and validates the configuration
before any request. With `new`, there are no local caps. These controls apply to
this automatic payment configuration, not direct calls to `Buyer`.

For InFlow charges, `max_amount` is in currency units and requires `currencies`.
For CARD, `max_amount` uses dollars with two decimal places, and `max_atomic_amount`
uses cents. For Tempo, `max_atomic_amount` uses token base units; `max_amount`
requires `max_amount_decimals`. Either Tempo amount cap requires `currencies`.
Amounts are strings, compared exactly without floating-point rounding.

InFlow and Tempo support `expected_recipients`; Tempo checks every split recipient
as well. Tempo additionally supports `allowed_chain_ids` and compares currency and
recipient addresses without regard to letter case. `None` leaves a restriction
unset; an empty allowlist rejects every value. A cap of `"0"` is a real zero cap.
Limits filter offers in the caller's preference order and are checked again before
credential creation. Request-specific options cannot weaken them. Server approvals
and policies still apply to every payment.

```no_run
# use inflow_mpp_buyer::{Buyer, Error, HttpPayment, HttpPaymentMethod, MethodLimits};
fn limited(buyer: Buyer) -> Result<HttpPayment, Error> {
    HttpPayment::new_with_limits(buyer, HttpPaymentMethod::Inflow { instrument_id: None }, MethodLimits {
        max_amount: Some("0.50".into()),
        currencies: Some(vec!["USDC".into()]),
        ..Default::default()
    })
}
```

### Structured errors

Normal callers handle upstream's `HttpError`. An InFlow credential-creation error
includes its code and message, plus request and transaction identifiers when
available. An optional `with_error_handler` callback receives the challenge and
original structured InFlow `Error` before conversion:

```no_run
# use inflow_mpp_buyer::HttpPayment;
fn observe(payment: HttpPayment) -> HttpPayment {
    payment.with_error_handler(|challenge, error| {
        eprintln!("Payment {} failed: {}", challenge.id, error.code);
        // Application code can inspect error.http_status, error.request_id, and error.body.
    })
}
```

The callback is synchronous and must not block or panic. It observes InFlow
credential failures, not resource HTTP errors or upstream challenge-parser errors.
It does not retry a payment. A missing wallet response can leave payment outcome
unknown; reconcile the original operation instead of assuming it failed.
Successful payment followed by a resource error likewise does not trigger a
refund or a second payment in this adapter.

The native provider is private, so this HTTP integration cannot be passed to
upstream's automatic MCP payment loop:

```compile_fail
use inflow_mpp_buyer::HttpPayment;
fn requires_provider<P: mpp::client::PaymentProvider>() {}
requires_provider::<HttpPayment>();
```

## Obtain a credential manually

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
chosen challenge. The manual Buyer does not buffer streams, follow redirects, or
retry resource delivery for you.

For MCP, obtain the `PaymentChallenge` from the payment-required tool response,
choose the offer, and use the same Buyer lifecycle. Your MCP client's payment
metadata handling must serialize the complete returned credential, including its
challenge description. No concrete MCP client dependency is required by this crate.

### Automatic MCP payment limitation

Use the manual Buyer lifecycle for MCP payments. Upstream `mpp` 0.20.0's
`McpPayment` tries another offered payment after an ordinary provider error,
including a lost response from a remote wallet. The first payment may already
have been accepted, so the next attempt could pay for the same tool call again.
Its terminal uncertainty errors require a blockchain transaction hash or describe
a session operation; neither represents every one-time payment through a remote
wallet.

This is tracked in [mpp-rs issue #727](https://github.com/tempoxyz/mpp-rs/issues/727).
Do not adapt the manual Buyer into upstream's automatic MCP retry loop while
this limitation applies. After an uncertain result, reconcile the original
payment instead of trying another offer. The existing manual HTTP and MCP APIs
remain usable; this limitation concerns automatic alternative-offer retries.

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

## Visa CARD purchases

Call `prepare_card` with `CardPaymentOptions` rather than `prepare`. Supply a
`Merchant` with its name, absolute HTTP/HTTPS URL, and two-letter country code.
`instrument_id: None` uses the account's primary card; `Some(id)` selects that
linked card. InFlow checks ownership, Visa eligibility and an unexpired allowance
covering the purchase. The SDK does not substitute another card after a rejection.

```no_run
use inflow_mpp_buyer::{Buyer, CardPaymentOptions, Merchant, PaymentChallenge, CancellationToken, WaitOptions};
async fn buy(buyer: &Buyer, challenge: &PaymentChallenge) -> Result<(), inflow_mpp_buyer::Error> {
    let payment = buyer.prepare_card(challenge, CardPaymentOptions {
        merchant: Merchant { name: "Example shop".into(), url: "https://shop.example".into(), country_code: "US".into() },
        instrument_id: None,
    }, &CancellationToken::new()).await?;
    let credential = payment.wait(WaitOptions::default()).await?;
    // Send the complete credential to the original Seller; readiness is not settlement.
    let _header = inflow_mpp_buyer::encode_credential(&credential)?;
    Ok(())
}
```

The returned `Payment` uses the same approval, wait and cancellation lifecycle.
The returned credential must match the entire requested challenge. Its encrypted
payload remains opaque; the SDK neither decrypts it nor rewrites a mismatch.
Stripe token creation is not an InFlow Buyer operation; Stripe Seller offers need
an external Stripe-capable payer.

## Read settlement status

`buyer.get_payment_status(transaction_id, PaymentStatusOptions::default(), &token)`
reads the original payment and returns its JSON snapshot, including `status` and
any `nextAction`. It sends one request by default; `retries` explicitly permits
up to three additional attempts. Each call reads a fresh snapshot.

`PENDING` or a ready credential does not mean settlement. An `authenticate_card`
action contains the dashboard URL for the buyer to complete verification. The SDK
returns it without opening it or sending credentials there. Cancelling a status
read does not cancel the payment. A failed read—including 404—is not permission to
create a replacement purchase. Retain the original transaction ID and credential.

## Errors

`Error::code` distinguishes `MPP_PAYMENT_CANCELLED`, `MPP_PAYMENT_TIMEOUT`,
`MPP_PAYMENT_EXPIRED`, `MPP_PAYMENT_FAILED`, and `MPP_MALFORMED_CREDENTIAL`.
Payment failures retain the platform problem in `error.body["problem"]` (null when
absent) and the supplied transaction identifier in `error.body["transactionId"]`.
Timeout and transaction-expiry errors also retain `transactionId` when supplied.
Use that identifier to investigate the original payment; do not create a replacement
payment merely because an operation failed. Transport
and authentication errors retain their core error codes and HTTP metadata.
Do not log credentials or payment payloads.

## Upstream automatic-client compatibility

`HttpPayment` connects the buyer to the native automatic HTTP client. That client
parses headers before invoking our provider and therefore bypasses our corrected
header parsing. [Issue #556](https://github.com/tempoxyz/mpp-rs/issues/556) tracks
HTTP quoted-string handling; [issue #730](https://github.com/tempoxyz/mpp-rs/issues/730)
tracks normalization of invalid explicit Authorization and empty header selections.
Our provider rejects unsupported values that remain present, but cannot distinguish
a value erased upstream from valid omission. Use `inflow_mpp::parse_challenges` and
the explicit buyer workflow when that original wire distinction must be checked.
