# inflow-x402-axum

Axum and Tower routes backed by the InFlow facilitator. Use `payment_layer` with
a route built by `inflow_x402_seller::Seller::route`. Apply the returned layer to
the specific protected Axum route using `get(handler).route_layer(layer)`. Using
`route_layer` leaves unmatched methods to Axum's 405 response instead of challenging
them for payment. Its resource URL must be the public URL buyers
will request, not the InFlow platform URL.

```no_run
use axum::{Router, routing::get};
use inflow_x402_axum::payment_layer;
use inflow_x402_seller::{Seller, OfferOptions, CancellationToken};

# async fn example(seller: Seller) -> Result<Router, Box<dyn std::error::Error>> {
let route = seller.route(&OfferOptions::new("$0.01"),
    &CancellationToken::new()).await?;
let layer = payment_layer(seller.facilitator(), route,
    "https://service.example/report")?;
let app = Router::new().route("/report",
    get(|| async { "Your report" }).route_layer(layer));
# Ok(app)
# }
```

The layer uses `x402-axum` to verify the payment, execute the handler, and settle
after a successful handler response. A handler returning a 4xx or 5xx response
does not trigger settlement. A settlement failure does not return the handler's
successful response. Keep irreversible fulfillment out of the handler unless the
application separately reconciles payment outcomes.

Payment challenges and HTTP 412 responses use `Cache-Control: no-store`.
Responses carrying a payment receipt add `private` unless it is already present,
preserving the handler's other cache directives. This matches the Node payment
adapter's cache policy. Routes outside this layer are unaffected.

This policy is applied by the InFlow layer: upstream `x402-axum` 2.0.2 does not
automatically set these cache headers. That omission is tracked in
[upstream issue 135](https://github.com/x402-rs/x402-rs/issues/135). Using upstream
middleware directly requires the application to supply its own cache policy.
The InFlow layer recognizes `private` only outside quoted header values; text
inside an extension such as `example="a, private, b"` does not make a response private.

On a failed settlement, upstream returns HTTP 402 without a `Payment-Response`
receipt header. Buyers therefore cannot decode a structured failure receipt from
that response. Successful settlements include the receipt header. This limitation
is tracked in [upstream issue 134](https://github.com/x402-rs/x402-rs/issues/134);
the lower-level facilitator's `settle` method retains the structured result.

Automatic `upto` routes are rejected because upstream middleware settles the full
authorized ceiling, not measured usage. See [upstream issue 72](https://github.com/x402-rs/x402-rs/issues/72).
Use the Seller facilitator's separate verification and settlement methods when
your application manages measured billing itself.

The returned layer supports `with_description`, `with_mime_type`, and
typed custom `with_extension` declarations. The factory adds the optional
payment identifier and carries the Seller's gas-sponsorship declarations.

See [inflow-x402-seller](https://github.com/inflowpayai/inflow-rust/tree/main/crates/inflow-x402-seller)
for Seller configuration and account prerequisites.
