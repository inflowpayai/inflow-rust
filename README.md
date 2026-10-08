# InFlow Rust SDK

Rust crates for InFlow MPP, x402, and TAP integrations. The workspace separates shared
configuration, protocol integration, and Buyer and Seller roles.

See the shared [SDK compatibility and support policy](https://github.com/inflowpayai/inflow-specs#sdk-compatibility-and-support)
for supported releases, dependency expectations, and security reporting.

Start with the [runnable Sandbox examples](examples/README.md) for account setup,
exact commands, approval waiting, receipt inspection, and failure handling.

## Crate layout

| Crate                                                       | Responsibility                                                                                   |
| ----------------------------------------------------------- | ------------------------------------------------------------------------------------------------ |
| [`inflow-core`](crates/inflow-core/README.md)               | Shared InFlow environment and client configuration.                                              |
| [`inflow-tap-seller`](crates/inflow-tap-seller/README.md)   | Independent TAP request verification, trusted-key caching, and replay protection.                |
| [`inflow-mpp`](crates/inflow-mpp/README.md)                 | MPP codecs, method-field validation, and shared InFlow protocol integration.                     |
| [`inflow-mpp-buyer`](crates/inflow-mpp-buyer/README.md)     | Payment creation, approval polling, subscription authorization, and cancellation for MPP Buyers. |
| [`inflow-mpp-seller`](crates/inflow-mpp-seller/README.md)   | Signed offers, validation, and settlement for InFlow, Tempo, Stripe, and Visa CARD charges.        |
| [`inflow-x402`](crates/inflow-x402/README.md)               | InFlow integration with the x402 protocol.                                                       |
| [`inflow-x402-buyer`](crates/inflow-x402-buyer/README.md)   | Buyer integration for InFlow x402 payments.                                                      |
| [`inflow-x402-seller`](crates/inflow-x402-seller/README.md) | Seller configuration, offers, verification, and settlement for x402 payments.                    |
| [`inflow-x402-axum`](crates/inflow-x402-axum/README.md)     | Optional Axum/Tower protected routes using upstream x402 middleware.                             |

## Environments

Payment Seller integrations require an InFlow **Seller** account and its dashboard API key.
Buyer integrations accept accounts permitted to buy; Sellers can also act as Buyers.
Register in [Sandbox](https://sandbox.inflowpay.ai) for testing or
[production](https://app.inflowpay.ai) for live payments. Credentials are separate
between environments. API keys authorize requests; they do not fund wallets.

TAP verification requires no InFlow account or credentials. Add only
`inflow-tap-seller` when verifying agent signatures without payments; it does not
depend on the MPP or x402 crates. See its [integration guide](crates/inflow-tap-seller/README.md)
and the [TAP example](examples/README.md#tap-seller).

`inflow_core::Environment` selects the InFlow API environment:

- `Environment::Production` (the default): `https://api.inflowpay.ai`.
- `Environment::Sandbox`: `https://sandbox.inflowpay.ai`.

## Repository verification

Rust 1.93 or newer is required. The toolchain file selects the minimum supported
compiler. CI runs the same checks on that compiler and current stable Rust.

Install the verification tools:

```sh
cargo install cargo-deny --version 0.19.8 --locked
cargo install cargo-llvm-cov --version 0.8.7 --locked
```

Run `make verify` for formatting, Clippy, tests, documentation, package construction
and compilation, dependency policy, coverage, and adapter tooling tests. Node.js 24
is required for the tooling tests. Run `make format` to format code.
Coverage requires at least 99% of executable source lines in each file, and 99% of
lines, functions, and regions overall; the goal is 100%. Test files are excluded
from the coverage report. The tool also excludes `examples/`; its application
tests run in the workspace but are not represented by SDK coverage percentages.
Codecov receives the same `lcov.info` report and enforces 99% project and patch coverage.

The workspace uses coordinated crate versions. `Cargo.lock` records the dependency
tree tested by CI; consumers resolve dependencies through each crate's manifest.

The [shared conformance checks](conformance/README.md) run public SDK operations
against pinned InFlow contracts and local HTTP fixtures. CI retains the reports,
including explicitly unsupported capabilities, for minimum and stable Rust.

The [Node interoperability checks](interop/README.md) exchange payments between
Rust and Node Buyers and Sellers over HTTP, using a synthetic InFlow platform.

## References

Maintainers: see [release preparation and publishing](RELEASING.md) for the manual
workflow, first-publication token setup, and Trusted Publishing configuration.

For differences from upstream MPP credential and header handling, see the
[MPP compatibility notes](crates/inflow-mpp/README.md#differences-from-upstream-mpp-014).

- [InFlow](https://app.inflowpay.ai)
- [InFlow SDK contracts](https://github.com/inflowpayai/inflow-specs)
- [Machine Payments Protocol](https://mpp.dev)
- [x402](https://www.x402.org)

## License

MIT. See [LICENSE](LICENSE).
