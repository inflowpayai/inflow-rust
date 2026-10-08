# inflow-mpp-seller

Accept MPP payments for a Service through InFlow. Create a signed challenge for
an endpoint, validate the buyer's credential, and settle before delivering the
paid resource. Supported methods are InFlow, Tempo, Stripe, and CARD charge.

[Source](https://github.com/inflowpayai/inflow-rust/tree/main/crates/inflow-mpp-seller) ·
[MPP](https://mpp.dev/) · [Core codecs](../inflow-mpp/README.md)

Add `inflow-mpp-seller`, `inflow-mpp`, and `serde_json` to use the examples below.
The handler can run inside any HTTP framework using the application's Tokio runtime.

## Account and setup

For a runnable HTTP server and matching Buyer, follow the
[Sandbox example walkthrough](https://github.com/inflowpayai/inflow-rust/tree/main/examples#mpp).

Use an InFlow **Seller** account and its API key. A Developer account does not
authorize Seller configuration, validation, or settlement.
[Sandbox](https://sandbox.inflowpay.ai) and [production](https://app.inflowpay.ai)
have separate accounts and credentials. Select `Environment::Sandbox` for testing;
production is the default.

The challenge-signing secret belongs to your Service and is separate from the
InFlow API key. Use a randomly generated secret of at least 32 bytes, store it
securely, and share it across instances that accept the same challenges.
Changing it invalidates outstanding challenges.

```no_run
use inflow_mpp_seller::{
    Authentication, CancellationToken, ChallengeOptions, ClientOptions,
    Environment, Method, Offer, Seller, SellerOptions,
};
use serde_json::json;

async fn configure() -> Result<Offer, Box<dyn std::error::Error>> {
    let seller = Seller::new(
        ClientOptions {
            environment: Environment::Sandbox,
            authentication: Authentication::ApiKey(std::env::var("INFLOW_API_KEY")?),
            ..Default::default()
        },
        SellerOptions {
            realm: "api.example.com".into(),
            secret_key: std::env::var("MPP_SECRET_KEY")?,
        },
        &CancellationToken::new(),
    ).await?;
    Ok(seller.offer(
        Method::Inflow,
        json!({"amount": "0.10", "currency": "USDC"}),
        ChallengeOptions {
            description: Some("One search request".into()),
            requires_auth: true,
            ..Default::default()
        },
    )?)
}
```

`Seller::new` loads authenticated Seller configuration before returning. If it
fails, handle the error and retry construction when appropriate. Clone the
constructed Seller for concurrent routes; clones reuse its configuration.
Use your application's Tokio runtime; the SDK does not start another runtime.

## Set the price and payment method

An `Offer` holds the endpoint's expected terms. Construct it from trusted Service
configuration, not from the buyer's submitted credential.

- InFlow amounts are decimal strings, such as `"0.10"`. Configuration supplies
  the Seller recipient and supported currency rails. If a currency supports
  multiple rails, specify `methodDetails.rail`. Provide `instrumentId` when the
  advertised instrument rail requires it.
- Tempo amounts are base-unit strings, not decimal currency amounts. Supply the
  token address in `currency` and the receiving address in `recipient`.
  `methodDetails` defaults to `feePayer: false` and `supportedModes: ["pull"]`.

The SDK checks the credential against the Offer, including rail and
method-specific transfer terms, before calling InFlow. Upstream MPP verifies
the signed challenge, expiration, and any request-body digest.

## Stripe tokens and Visa CARD

Use `Method::Stripe` to accept an external payer's Stripe Shared Payment Token.
Use `Method::Card` to accept an encrypted Visa network-token credential. Neither
method decrypts tokens or contacts Stripe from your application: InFlow performs
credential verification and processing through the existing validate/broadcast flow.

Both methods accept **decimal USD strings**, from `"0.50"` through `"999999.99"`.
For example, `seller.offer(Method::Card, json!({"amount":"1.25"}), options)`
creates a challenge containing `"amount":"125"` in cents. Extra fractional digits
are rejected, not rounded. This differs from Tempo's base-unit input.

Authenticated Seller configuration supplies Stripe's network profile and allowed
payment methods, or CARD's merchant recipient, name, Visa network and public
encryption key. Route values cannot override these fields. An unavailable
capability prevents offer construction. Your application needs an InFlow Seller
key, not a Stripe secret key.

- Both methods accept `externalId` of up to 255 characters, including an empty string.
- Stripe accepts top-level `metadata`, placed inside `methodDetails` on the wire:
  up to 45 string entries, keys up to 40 characters and values up to 500 characters.
  Keys cannot be blank, contain brackets, or use the reserved names `externalId`,
  `inflowMppTransactionId`, `mppChallengeId`, `mppIntent`, `mppMethod`, or `stripeNetworkProfile`.
  A supplied challenge reference must match the credential's `externalId` exactly.
- CARD accepts top-level `billingRequired`; omission and explicit `false` remain distinct.

Both methods require a successful receipt with the same method and challenge ID.
An absent payer `source` is represented as an empty string in the InFlow request;
external payers do not need an InFlow payer identity. Token contents and optional
billing fields are retained. See the [runnable examples](../../examples/README.md#card-and-stripe).

## Handle an HTTP request directly

`requires_auth: true` puts payment in `Payment-Authorization`, leaving
`Authorization` for your Service's own authentication. It does **not** implement
or enforce that authentication: authenticate and authorize the caller before
accepting payment.

This framework-independent function shows the payment portion of a handler.
Its return value contains the response status, payment header, and body for your
framework to write. Read the incoming body with your framework's size limit and
pass the exact bytes, without parsing and re-serializing them.

```no_run
use inflow_mpp_seller::{
    CancellationToken, Error, Offer, decode_credential, render_challenge,
};

async fn payment_response(
    offer: &Offer,
    payment_authorization: Option<&str>,
    body: &[u8],
    cancellation: &CancellationToken,
) -> Result<(u16, &'static str, String, &'static str), Error> {
    let Some(header) = payment_authorization else {
        let challenge = offer.challenge(Some(body))?;
        return Ok((402, "WWW-Authenticate", render_challenge(&challenge)?, ""));
    };
    let (_, encoded) = header.split_once(' ')
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Payment"))
        .ok_or_else(|| Error::new("INVALID_MPP_DATA", "expected Payment credential"))?;
    let credential = decode_credential(encoded.trim())?;
    let receipt = offer.accept(&credential, Some(body), cancellation).await?;
    let header = inflow_mpp::encode(&serde_json::to_value(receipt)
        .map_err(|_| Error::new("INVALID_MPP_DATA", "invalid receipt"))?)?;
    // Run the paid operation only after accept returns a receipt.
    Ok((200, "Payment-Receipt", header, "paid result"))
}
```

Read `Payment-Authorization` for the configuration above, or `Authorization`
when `requires_auth` is false. On the first request, return 402 with the rendered
challenge. On the retried request, decode the payment credential and call
`accept`. Attach its encoded receipt to the successful resource response.
Use `Cache-Control: no-store` for payment challenges and payment responses.

Map errors deliberately: rejected payment is not a successful resource response,
and a network or platform failure must not invite the buyer to make another
payment automatically. `Error` retains the platform's HTTP metadata and problem
body. Never log the payment credential, API key, or challenge-signing secret.

## Validation, settlement, and cancellation

`validate` is non-mutating and returns payment details; it is **not** proof of
settlement and does not authorize resource delivery. `accept` performs validation
again and then calls the terminal settlement operation. Do not deliver the paid
resource based only on a prior `validate` result.

For an InFlow instrument payment, `accept` also requires the receipt's method and
challenge identifier to match the submitted credential. A missing or mismatched
identifier is an error, not permission to deliver the resource or charge again.

When the platform enables idempotency keys, each acceptance call generates one
key and retains it across that call's transport retries. A separate call gets
a separate key. The platform remains authoritative for credential replay.
Both configurations use the shared client's bounded retry policy.

Cancellation stops local requests; it is not payment reversal. A lost settlement
response, a cancelled request, or a handler failure after settlement can leave
a payment completed. Do not automatically charge again to recover resource
delivery. Your application owns delivery recovery.

Challenges expire five minutes after creation unless `expires` is supplied.
Keep the same body bytes for challenge creation and acceptance when using body
binding. Pass `None` only when the route deliberately does not bind payment to
request-body contents.

## Upstream differences and limitations

- The SDK preserves the full credential, including the challenge description,
  while using upstream MPP for verification. `mpp 0.14.0`'s credential projection
  omits that field; [upstream PR #490](https://github.com/tempoxyz/mpp-rs/pull/490)
  addresses it. Do not convert incoming credentials to that lossy type before
  passing them to this SDK.
- Expected-offer checks include InFlow and Tempo method-specific terms.
  [Upstream issue #555](https://github.com/tempoxyz/mpp-rs/issues/555) tracks the
  corresponding upstream binding support.
- Seller subscriptions are not exposed because upstream lacks the Seller intent
  implementation. [Issue #554](https://github.com/tempoxyz/mpp-rs/issues/554)
  tracks it. Buyer subscription support is separate.
- Upstream `PaymentBodyLayer` in `mpp 0.14.0` reads `Authorization` even when the
  challenge specifies `Payment-Authorization`. It cannot combine that separate
  payment header with automatic body verification. The direct handler above
  supports both without rewriting Service authentication headers.
  [Upstream PR #419](https://github.com/tempoxyz/mpp-rs/pull/419) contains the fix;
  it is absent from the dependency version used here. Direct integration supports
  body binding and separate Service authentication without that layer.
- Upstream middleware in `mpp 0.14.0` turns every verification failure into a
  fresh 402 challenge, including platform or transport failures. Use the direct
  handler path to preserve the distinction between rejected credentials and
  unavailable payment infrastructure. [Upstream PR #497](https://github.com/tempoxyz/mpp-rs/pull/497)
  corrects the middleware's error handling. Do not automatically pay again after
  an ambiguous settlement failure.

This crate exposes direct handler integration, not an automatic Tower or Axum
payment layer. That keeps platform errors available to the application while the
released upstream middleware has the limitations above. Neither limitation
requires the application to implement payment verification itself.
