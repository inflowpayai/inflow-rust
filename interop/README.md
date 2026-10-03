# Rust and Node interoperability

These checks run Rust Buyers against Node Sellers and Node Buyers against Rust
Sellers over loopback HTTP. Peers use the public SDKs and their payment middleware.
The InFlow platform is a synthetic HTTP server; no accounts, live signing, or
settlement are involved.

## Run

Use Node 24 and a supported Rust compiler. Check out clean copies of `inflow-node`
and `inflow-specs` at the commits in `node.lock.json` and
`../conformance/inflow-specs.lock.json`. In the Node checkout, run
`pnpm install --frozen-lockfile` and `pnpm build`. From this repository:

```sh
make verify
node scripts/interoperability.mjs ../inflow-node /tmp/rust-node-report.json ../inflow-specs
```

The report path must not exist. Reports record source revisions, dirty state,
resolved Rust dependencies, Node package versions, and each case's observed
platform requests. CI retains reports for minimum and stable Rust. The test-only
peer is not included in published crates.

## What is checked

- MPP InFlow and Tempo charges in both directions; Rust subscription creation
  and existing-subscription use against Node Sellers.
- x402 balance and exact payments in both directions, using Rust `HttpBuyer`
  and Axum payment middleware, and Node's HTTP client and Express middleware.
- Immediate and pending approvals, verification rejection, settlement failure,
  and protected-handler failure.
- One purchase per request, application-header preservation, no platform API
  key sent to the merchant, receipt identity, and verification/settlement order.
- Deliberately corrupted receipts must fail the harness assertions in all four
  protocol/direction combinations.

MPP settles before calling the protected handler. x402 verifies before the handler
and settles after a successful handler response. These are separate assertions.
The Rust MPP peer supplies its application HTTP route because the Seller package
exposes `Offer::accept`, not web middleware. Validation and settlement remain inside
that SDK method; the peer does not implement payment verification.

Rust MPP Seller subscriptions are excluded because upstream `mpp` lacks the Seller
subscription intent. Rust Buyer subscriptions remain tested. The suite does not
prove live platform authorization, blockchain execution, external-wallet signing,
MCP interoperability, Stripe SPT, or TAP. Native tests, shared conformance, and
separately authorized live payment checks serve different purposes.
