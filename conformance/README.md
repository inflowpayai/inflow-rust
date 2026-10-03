# Shared SDK conformance

These development-only adapters call the public Rust Buyer, Seller, and codec APIs
against the cases in `inflow-specs`. They do not run live payments and are not
dependencies of the published crates.

## Run

Install Node.js 24, pnpm 11.20.0, and the Rust toolchain selected by this repository.
Use a clean `inflow-specs` checkout at the exact commit in
[`inflow-specs.lock.json`](inflow-specs.lock.json). Install its tooling dependencies
with `pnpm install --frozen-lockfile` in that checkout. Then, from this repository:

```sh
mkdir /tmp/inflow-rust-reports
node scripts/conformance.mjs \
  --contract-root ../inflow-specs \
  --output-dir /tmp/inflow-rust-reports
```

Use a fresh output directory for each run. Existing report files are never
overwritten. Each suite saves its case inputs, capability declarations, resolved
workspace dependency versions, and report. Reports identify both Git revisions
and dirty states; release evidence requires clean checkouts. CI retains these
artifacts for both the minimum and stable Rust compilers.

## What is exercised

- Runtime: public Buyer/Seller construction or payment preparation, environment
  destinations, authentication headers, structured and empty HTTP errors,
  redirect refusal, and operation-specific retries. Environment tests capture
  destinations without contacting public servers. Rust uses a public custom
  transport, not a configurable API base URL. Seller capability setup is supplied
  separately when measuring configuration errors.
- MPP: codecs, Buyer charge and subscription flows, approval cleanup, Seller
  charge preparation, signed credential validation, settlement, and changed route
  terms. The route cases send an HTTP request through an Axum test handler that
  calls `Offer::accept`; the handler does not implement payment validation.
- x402: identifiers, Buyer signing and cleanup, Seller offers, verification,
  settlement, pending retries, and sponsoring declarations. Offer cases supply
  configuration through the public transport and project the typed offer into
  the shared result shape; they do not rebuild offer calculations.

HTTP cases use the runner's loopback server through `Transport`. The adapter
refuses external destinations, disables redirects and transport retries, and
leaves SDK retry/cancellation decisions to the SDK. It never receives expected
results or platform response scripts. Offer-only cases use supplied configuration
instead of a server. MPP fixture challenges are signed with a fixed test secret
by the upstream challenge factory, without changing negative-case mutations.
These shared cases exercise the custom transport; the SDK's native HTTP tests
separately verify its default transport, including cancellation and redirects.

## Explicit unsupported capabilities

MPP Seller subscriptions are unsupported because upstream `mpp` does not implement
the subscription Seller intent. Buyer subscriptions remain tested. x402 concurrent
waiting on the same handle is unsupported because `Payment::wait` consumes that
handle; its compile-fail documentation test verifies the ownership restriction.
The consuming-handle fixture profile requires cleanup after failed waits.

Both omissions appear in reports as skipped, never passed. All other selected
cases are mandatory. Stripe SPT and TAP are outside these suites. These reports
do not claim live platform interoperability or cross-language payment exchange.

`make verify` also tests the adapter message boundary and runner configuration.
The adapter is a test executable, excluded from SDK source coverage by the same
test-file rule as other tests; conformance execution does not replace SDK coverage.
