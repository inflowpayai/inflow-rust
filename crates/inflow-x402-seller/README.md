# inflow-x402-seller

Build x402 payment offers from your InFlow Seller configuration and verify or
settle payments through the InFlow facilitator. This crate does not require Axum;
use `inflow-x402-axum` to protect Axum routes with the upstream middleware.

## Prerequisites

For a runnable Axum server and matching Buyer, follow the
[Sandbox example walkthrough](https://github.com/inflowpayai/inflow-rust/tree/main/examples#x402).

Create an InFlow **Seller** account and generate its API key:

- [Sandbox registration](https://sandbox.inflowpay.ai) for testing.
- [Production registration](https://app.inflowpay.ai) for live payments.

`Seller::new` requires that API key and loads configuration and facilitator
capabilities before returning. A Developer account is not a Seller account.
`Facilitator::new` also supports anonymous use when you only need verification,
settlement, and supported capabilities; it does not load Seller configuration.

## Create payment offers

```no_run
use inflow_x402_seller::{Authentication, CancellationToken, ClientOptions,
    Environment, OfferOptions, Seller};

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let seller = Seller::new(ClientOptions {
    environment: Environment::Sandbox,
    authentication: Authentication::ApiKey(std::env::var("INFLOW_API_KEY")?),
    ..Default::default()
}, &CancellationToken::new()).await?;

let route = seller.route(&OfferOptions::new("$0.01"),
    &CancellationToken::new()).await?;
# let _ = route;
# Ok(())
# }
```

Prices accept `$0.01`, `0.01 USDC`, or a `Price` with separate `amount` and
`currency` fields. An explicit currency overrides one in the amount string.
For balance and blockchain offers, USD selects all configured stablecoin currencies. Conversion uses decimal strings,
not floating-point numbers; nonzero digits that would be truncated are rejected.
Prices support up to eight decimal places. Amounts in returned offers are atomic
units, with each configured asset or payment method's decimal scale.

`OfferOptions` defaults to a 300-second timeout and configured balance/blockchain
schemes and networks. Optional scheme and network lists intersect; an empty list
matches nothing. `permit2: true` selects compatible Permit2 on-chain offers without
removing balance offers. No matching configuration produces an empty offer list;
the Axum adapter rejects an empty protected route.

Linked-card offers require `"instrument"` in `OfferOptions::schemes` and an
advertised instrument method in Seller configuration. Their price must be fiat USD,
at least $0.50, and represent exact whole cents no greater than
9,223,372,036,854,775,807 cents. They do not require blockchain assets or wallets.
The offer's wire amount still uses the configured method's decimal scale, not a
hard-coded two-decimal scale. Network filters continue to apply.

`offers` builds the accepted requirements. `route` also advertises gas sponsorship
when every selected Permit2 offer and the facilitator support it. EIP-2612 is
preferred over InFlow EIP-7702 when both apply. The Seller passes Buyer sponsorship
data to the facilitator; it does not sign wallet authorizations itself.

## Verification and settlement

`seller.facilitator()` returns a clone sharing its capability cache and implements
the upstream `x402_types::facilitator::Facilitator` trait. The inherent `verify`
and `settle` methods accept a cancellation token. Use
`inflow_x402::facilitator_request` to construct their request from a payment payload
and the server's selected requirements.

Verification is not settlement: a successful verification does not charge the
Buyer. Inspect `isValid` before fulfilling a request, and `success` before treating
settlement as complete. Responses retain additional fields. A valid payment
identifier is preserved; an absent identifier is derived from payment material.
Only HTTP 409 `idempotency_pending` causes settlement retries, with the same request
and identifier, up to five attempts. Cancellation stops waiting; it is not a refund.

Configuration and supported capabilities are cached for one hour. Concurrent
refreshes share a successful result; a failed refresh does not replace the previous
snapshot. Use `refresh_config` and `refresh_supported` explicitly after configuration
changes. `signer_addresses` checks an exact network before its namespace wildcard.
Offers and route layers are snapshots: refreshing the client does not rewrite an
existing layer. Rebuild affected routes when changing their advertised payment terms.

## Differences from upstream integration

- InFlow's client selects platform URLs, handles account credentials, preserves
  payment identifiers, and applies the bounded pending-settlement retry above.
  The API key is sent to InFlow, not to the Buyer or a merchant resource URL.
- Offer construction uses your Seller's configured assets, wallets, and payment
  methods, rather than requiring you to hard-code their atomic amounts and metadata.
- Automatic metered routes are not supported. Upstream `x402-axum` settles the full
  authorized ceiling rather than an amount measured by the handler
  ([issue 72](https://github.com/x402-rs/x402-rs/issues/72)). Explicitly selecting
  `upto` can produce offers for a custom application that separately verifies and
  settles a measured amount; the Axum adapter rejects those offers. Your application
  must bind that amount to its own usage calculation and the authorized ceiling.

The HTTP implementation uses Tokio. Custom InFlow transports receive credentials
and must not follow redirects or log secrets. See the
[workspace documentation](https://github.com/inflowpayai/inflow-rust) for verification commands.
