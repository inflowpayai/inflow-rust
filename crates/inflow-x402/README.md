# inflow-x402

Shared x402 V2 wire helpers for InFlow Buyers and Sellers, using
[`x402-types`](https://docs.rs/x402-types/2.0.2/x402_types/) from
[x402-rs](https://github.com/x402-rs/x402-rs).

```toml
[dependencies]
inflow-x402 = "0.4.0"
serde_json = "1"
```

This crate does not create approvals, sign payments, or settle them. Use
`inflow-x402-buyer` for Buyer workflows and `inflow-x402-seller` for Seller workflows.
Wire encoding is not proof of payment.

## Payment identifiers and HTTP headers

The payment-identifier extension declares `required: false`. A caller can attach an
identifier without changing that declaration. Identifiers contain 16–128 ASCII letters,
digits, underscores or hyphens. Use a fresh identifier for each distinct payment and
retain it when retrying that same payment.

```rust
use inflow_x402::{decode, encode, generate_payment_id, identifier_declaration, identifier_entry};

let id = generate_payment_id("pay_")?;
let entry = identifier_entry(&identifier_declaration(), &id).expect("valid generated identifier");
let header = encode(&entry);
assert_eq!(decode(&header)?, entry);
# Ok::<(), inflow_x402::Error>(())
```

`encode` and `decode` use standard padded Base64 JSON, not Base64url. They preserve
JSON fields, including unknown extensions and explicit nulls. They do not validate
signatures or decide whether an Offering has been paid for.

## Responses retain their complete data

`VerifyResponse` and `SettleResponse` re-export upstream's JSON-preserving wrappers.
Read fields through `.0`; check `isValid` or `success` before treating a response as
successful. A successful HTTP request can contain a rejected payment.

```rust
use inflow_x402::SettleResponse;
use serde_json::json;

let response = SettleResponse(json!({
    "success": false,
    "errorReason": "settlement_failed",
    "errorMessage": "Payment could not settle",
    "transaction": "",
    "network": "inflow:1"
}));
assert_eq!(response.0["success"].as_bool(), Some(false));
assert_eq!(response.0["errorReason"], "settlement_failed");
```

These are `x402_types::proto` wrappers, not the separate `proto::v2` response enums.
In x402-types 2.0.2, those enums omit some response fields and the typed settlement
failure decoder expects `error_reason` instead of the wire field `errorReason`.
Keeping the generic wrappers retains amounts, error messages and extension data and
matches upstream's `Facilitator` interface. No upstream fork is required.

`facilitator_request` builds an upstream V2 request while preserving the supplied
payment payload and requirements. It checks the version and object structure; it
does not compare payment terms, authorize a charge or perform verification.

## Runtime and dependency boundaries

The shared endpoint implementation uses `inflow-core` on the caller's Tokio runtime.
It is in a documentation-hidden `internal` module for the Buyer and Seller crates;
it is not a supported low-level endpoint client for integrators. Authentication,
redirect refusal, request timeouts and transport customization follow `inflow-core`.
This crate does not enable external-wallet signing or framework middleware features.

The internal facilitator calls preserve complete responses and never automatically
retry network failures, rate limits or server errors. Settlement retries only
`409 idempotency_pending`, at most five attempts with an unchanged payment identifier.
Cancellation stops waiting and further attempts; it does not reverse a submitted payment.

See the [workspace documentation](https://github.com/inflowpayai/inflow-rust)
for the crate layout, environments, and repository verification commands.
