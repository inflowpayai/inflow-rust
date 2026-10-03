# InFlow Rust SDK

Rust crates for InFlow MPP and x402 integrations. The workspace separates shared
configuration, protocol integration, and Buyer and Seller roles.

## Crate layout

| Crate | Responsibility |
| --- | --- |
| `inflow-core` | Shared InFlow environment and client configuration. |
| [`inflow-mpp`](crates/inflow-mpp/README.md) | MPP codecs, method-field validation, and shared InFlow protocol integration. |
| [`inflow-mpp-buyer`](crates/inflow-mpp-buyer/README.md) | Payment creation, approval polling, subscription authorization, and cancellation for MPP Buyers. |
| `inflow-mpp-seller` | Seller integration for InFlow MPP payments. |
| `inflow-x402` | InFlow integration with the x402 protocol. |
| `inflow-x402-buyer` | Buyer integration for InFlow x402 payments. |
| `inflow-x402-seller` | Seller integration for InFlow x402 payments. |

## Environments

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
and compilation, dependency policy, and coverage. Run `make format` to format code.
Coverage requires at least 99% of executable source lines in each file, and 99% of
lines, functions, and regions overall; the goal is 100%. Test files are excluded
from the coverage report. Documentation-only crates have no executable coverage;
that is not evidence that their payment integrations are implemented or tested.
Codecov receives the same `lcov.info` report and enforces 99% project and patch coverage.

The workspace uses coordinated crate versions. `Cargo.lock` records the dependency
tree tested by CI; consumers resolve dependencies through each crate's manifest.

## References

For differences from upstream MPP credential and header handling, see the
[MPP compatibility notes](crates/inflow-mpp/README.md#differences-from-upstream-mpp-014).

- [InFlow](https://app.inflowpay.ai)
- [InFlow SDK contracts](https://github.com/inflowpayai/inflow-specs)
- [Machine Payments Protocol](https://mpp.dev)
- [x402](https://www.x402.org)

## License

MIT. See [LICENSE](LICENSE).
