# inflow-mpp

MPP challenge, credential, and receipt handling for InFlow integrations. This crate
provides codecs and request validation; it does not initiate a payment, settle funds,
or verify the authenticity of a decoded credential.

[Source](https://github.com/inflowpayai/inflow-rust/tree/main/crates/inflow-mpp) ·
[MPP](https://mpp.dev/) · [Upstream Rust library](https://github.com/tempoxyz/mpp-rs)

## Decode a payment challenge

```rust
use inflow_mpp::{decode, parse_challenges, validate_request};

fn main() -> Result<(), inflow_mpp::Error> {
    let headers = [
        r#"Payment id="example", realm="shop.example", method="inflow", intent="charge", request="eyJhbW91bnQiOiIxIiwiY3VycmVuY3kiOiJVU0RDIn0""#,
    ];
    for challenge in parse_challenges(&headers)? {
        let request = decode(challenge.request.raw())?;
        validate_request(challenge.method.as_str(), challenge.intent.as_str(), &request)?;
        assert_eq!(request["amount"], "1");
    }
    Ok(())
}
```

Pass repeated `WWW-Authenticate` values together. Comma-combined Payment challenges
are returned in order; other authentication schemes are ignored. Malformed Payment
challenges return an error instead of silently disappearing. `render_challenge`
escapes quotes and backslashes and rejects header control characters.

## Preserve the credential

`Credential` contains the upstream `PaymentChallenge`, an open JSON-object payload,
and an optional payer `source`. `decode_credential` and `encode_credential` retain
the encoded `request` and `opaque` strings unchanged. Keep the original credential
for forwarding: reconstructing a challenge from decoded fields can change the
bytes used for verification. Credentials intentionally do not implement `Debug`.

`source` is optional in the general MPP envelope. InFlow-issued credentials include
it for payer binding; do not remove it from a platform response. Decoding checks the
envelope, not the signature, balance, ownership, or settlement state.

`decode_receipt` accepts successful receipts, validates required fields and the
timestamp, and retains method-specific extensions in the upstream `Receipt` type.
For example, `settlement` remains available through `receipt.extensions`.

## Method fields and amounts

`validate_request` supports InFlow charge and subscription requests and Tempo charge
requests. `validate_payload` checks the open InFlow payload or the proof field
selected by the Tempo payload type. Neither function changes caller-owned data.

InFlow amounts are decimal strings such as `"1.50"`; Tempo amounts are integer
strings in token base units such as `"1500000"`. Do not convert amounts through
floating-point numbers. Capability selection and whether a currency or funding
instrument is available depend on the platform configuration, not these shape checks.
Representing subscription request data does not establish Seller framework support.

`encode` uses upstream canonical JSON and base64url encoding, omitting null object
members to match the InFlow API. Null array elements are retained. Use this for new
request objects, not to rebuild an already-issued challenge's encoded fields.

## Differences from upstream mpp 0.14

- **Credential descriptions:** upstream `ChallengeEcho` drops `description`.
  This crate's credential retains the complete challenge. Upstream
  [PR #490](https://github.com/tempoxyz/mpp-rs/pull/490) contains a fix that is not
  included in mpp0.14.0. Description is display data, not a signed payment term.
- **HTTP quoted strings:** upstream interprets `\u0041` inside a quoted header as
  `A`. HTTP quoted-pair rules interpret it as `u0041`. This crate's header codec
  follows HTTP semantics, matching the Node integration. The mismatch is tracked
  in [issue #556](https://github.com/tempoxyz/mpp-rs/issues/556). Using upstream's
  header parser directly does not apply this correction. Protocol types and
  challenge signature verification remain upstream responsibilities.

The dependency disables upstream default features; using codecs alone does not
enable upstream wallet, Seller server, or framework integrations. The
documentation-hidden `internal` module contains shared platform calls for the
Buyer and Seller crates and is not a supported general-purpose endpoint API.
