# inflow-mpp-seller

Accept MPP payments for a Service through InFlow. Create a signed challenge for
an endpoint, validate the buyer's credential, and settle before delivering the
paid resource. Supported methods are InFlow, Tempo, Stripe, and CARD charge.

[Source](https://github.com/inflowpayai/inflow-rust/tree/main/crates/inflow-mpp-seller) ·
[MPP](https://mpp.dev/) · [Core codecs](../inflow-mpp/README.md)

Add `inflow-mpp-seller`, `inflow-mpp`, and `serde_json` to use the direct-handler examples below.
The handler can run inside any HTTP framework using the application's Tokio runtime.

## Request-time pricing

With the `tower` feature, `RequestPaymentLayer` resolves an offer asynchronously
from HTTP request parts. It delegates payment processing to `PaymentLayer`.

```no_run
# #[cfg(feature = "tower")]
# fn request_pricing(seller: inflow_mpp_seller::Seller) {
use http::request::Parts;
use inflow_mpp_seller::{RequestPaymentLayer, Method};
use serde_json::json;

let layer = RequestPaymentLayer::new(move |request: Parts| {
    let seller = seller.clone();
    async move {
        let amount = if request.uri.path() == "/reports/full" { "0.02" } else { "0.01" };
        seller.offer(Method::Inflow,
            json!({"amount":amount,"currency":"USDC","methodDetails":{"rail":"balance"}}),
            Default::default())
    }
});
# let _ = layer;
# }
```

For body-based pricing, use `RequestPaymentBodyLayer::new(|parts, bytes| async move { ... })`
and optionally `.max_body_bytes(limit)` (default 2 MiB). It binds payment to those
bytes and forwards them to the handler as `http_body_util::Full<bytes::Bytes>`.
The metadata-only layer does not read or transform the body. Response bodies must
support `Default + From<bytes::Bytes>`, including Axum's `Body`.

Resolution runs once per incoming request, including paid retries; it is not a
price lock across requests. Resolver failures return a generic 500 without a
challenge; oversized or unreadable bodies return 413 or 400 before resolution.
These layers return JSON Problem Details and never run the handler on pricing
failure. Authenticate before the payment layer when pricing uses customer identity.
Resolver errors remain available to server middleware through `response.extensions().get::<inflow_mpp_seller::Error>()`; they are not serialized to the client.
Fixed offers and direct `Offer::accept` handlers remain available.

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

## Tower middleware

Enable `features = ["tower"]` on `inflow-mpp-seller` to use an `Offer` with
upstream's `PaymentLayer` or `PaymentBodyLayer`. The layer calls `accept` before
the application handler and attaches a receipt to successful responses.
Use `with_problem_details` to return readable JSON payment errors instead of
empty error bodies. The callback converts those JSON bytes into your framework's
response-body type; it does not replace application responses.

The example below also uses `bytes`, `http`, and `tower` with its `util` feature.
It wraps a Tower service whose response body is a byte vector:

```no_run
# #[cfg(feature = "tower")]
# async fn example(offer: inflow_mpp_seller::Offer) -> Result<(), std::convert::Infallible> {
use bytes::Bytes;
use http::{Request, Response};
use inflow_mpp_seller::PaymentLayer;
use tower::{Layer, ServiceExt, service_fn};

let handler = service_fn(|_: Request<()>| async {
    Ok::<_, std::convert::Infallible>(Response::new(b"paid result".to_vec()))
});
let protected = PaymentLayer::new(offer)
    .with_problem_details(|json: Bytes| json.to_vec())
    .layer(handler);
let response = protected.oneshot(Request::new(())).await?;
# Ok::<(), std::convert::Infallible>(())
# }
```

`offer` is the trusted configuration constructed above. In Axum, use
`with_problem_details(axum::body::Body::from)` on the layer and install it on the
paid route, as demonstrated in the runnable Seller example. Authentication remains
your application's responsibility: when `requires_auth` is true, the layer reads
`Payment-Authorization` and leaves `Authorization` for your authentication layer.
Run authentication before the payment layer.

For body-bound payments, use `PaymentBodyLayer::new(offer).max_body_bytes(limit)`
before `.with_problem_details(...)`. It buffers at most that many bytes and passes
the original bytes to the handler. The default limit is 2 MiB. `PaymentLayer`
does not bind payments to request-body contents.

Malformed credentials, mismatched offers, and explicit payment rejections return
a payment problem with a fresh challenge. An explicit 403 payment rejection is
terminal and has no replacement challenge. Transport failures and unknown payment
outcomes return a generic 500 problem without backend details or a fresh challenge.
Neither kind of failure runs the handler. Direct `accept` calls retain the full
structured error for applications that need their own error presentation.

The layer does not retry the handler or reverse payment if the handler fails.
Upstream adds receipts only to successful application responses; a failed response
can therefore follow a completed payment without carrying a receipt. Reconcile
that payment before retrying, and implement fulfillment recovery in your application.

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

Malformed validation responses and missing or mismatched receipts return
`MPP_PAYMENT_FAILED` with an `internal-payment-error` problem and status 500 in
`Error.body`. Explicit platform problems retain their original fields. Do not map
every `MPP_PAYMENT_FAILED` to HTTP 402: inspect the problem status. An internal
failure during settlement can leave the payment outcome unknown; it is not an
instruction to create another payment.

- Expected-offer checks use upstream's `validate_expected_request` hook to bind
  method-specific terms before validation or settlement, including the selected
  InFlow rail, instrument, and Tempo transfer terms.
- Seller subscriptions are not exposed because upstream lacks the Seller intent
  implementation. [Issue #554](https://github.com/tempoxyz/mpp-rs/issues/554)
  tracks it. Buyer subscription support is separate.
