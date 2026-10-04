# Publishing InFlow Rust

All nine public crates use one workspace version. The release workflow is manual
and runs only from `main`. Merging a pull request does not publish anything.

Before version 1.0, incompatible public API changes require a minor increment;
compatible fixes use a patch increment. From version 1.0, use semantic versioning.
See the shared [SDK support policy](https://github.com/inflowpayai/inflow-specs#sdk-compatibility-and-support)
for maintenance of older releases.

## Prepare a version

Update the workspace version and every internal dependency requirement together.
Update `Cargo.lock`, merge the version change, and use the exact merged commit.
Rust 1.93.0 packages the release; Node 24 runs its scripts. CI also verifies stable
Rust and separate consumers with default, EVM, Solana, and combined features.

Open [Release](https://github.com/inflowpayai/inflow-rust/actions/workflows/release.yml),
select **Run workflow**, select **main**, and leave **publish** unchecked. This runs
the full repository gates, pinned shared conformance, Node interoperability, and
packaged-consumer checks. It uploads `release-packages` and `release-reports`.
The package artifact includes the nine `.crate` archives and `manifest.json`,
which records the commit, version, and SHA-256 checksum of each archive.

## First publication

Crates.io requires an API token to publish each crate name for the first time.
Trusted Publishing can be configured only after the crates exist. Perform these
steps only after the release has been approved and preparation is green.

This also applies when adding a crate to an already-published workspace. Each
crate, including `inflow-tap-seller`, needs its own first publication and Trusted
Publisher configuration. An existing publisher for `inflow-core` does not cover
the other crate names.

1. Sign in at [crates.io](https://crates.io) with the GitHub account that will own
   these crates, and verify its email address in [account settings](https://crates.io/settings/profile).
2. Create a short-lived token at [API tokens](https://crates.io/settings/tokens).
   Grant publication access for the nine crate names listed below. Do not put
   the token into a command argument, source file, chat message, or GitHub secret.
3. Check out the prepared `main` commit with a clean tree. Download and extract
   `release-packages` into a fresh directory. The scripts require that exact commit.
4. In a local terminal, enter the token without echoing it, then publish:

```sh
read -s CARGO_REGISTRY_TOKEN
export CARGO_REGISTRY_TOKEN
node scripts/release.mjs publish /absolute/path/to/release-packages
unset CARGO_REGISTRY_TOKEN
node scripts/verify-consumer.mjs registry /absolute/path/to/release-packages
```

Unset the token even if publication fails, then revoke it in crates.io. Publication
is sequential and not atomic. If interrupted, inspect the crate pages and rerun
from the same commit with the same artifacts. The script accepts an existing
version only when its registry checksum matches the prepared archive and it is
not yanked. A checksum mismatch stops publication; it is not automatically repaired.

## Configure Trusted Publishing

For each crate, open its **Settings → Trusted Publishing**, select GitHub, and use:

| Field             | Value           |
| ----------------- | --------------- |
| Repository owner  | `inflowpayai`   |
| Repository name   | `inflow-rust`   |
| Workflow filename | `release.yml`   |
| Environment       | `release`       |

- [inflow-core](https://crates.io/crates/inflow-core/settings)
- [inflow-tap-seller](https://crates.io/crates/inflow-tap-seller/settings)
- [inflow-mpp](https://crates.io/crates/inflow-mpp/settings)
- [inflow-mpp-buyer](https://crates.io/crates/inflow-mpp-buyer/settings)
- [inflow-mpp-seller](https://crates.io/crates/inflow-mpp-seller/settings)
- [inflow-x402](https://crates.io/crates/inflow-x402/settings)
- [inflow-x402-buyer](https://crates.io/crates/inflow-x402-buyer/settings)
- [inflow-x402-seller](https://crates.io/crates/inflow-x402-seller/settings)
- [inflow-x402-axum](https://crates.io/crates/inflow-x402-axum/settings)

The GitHub [release environment](https://github.com/inflowpayai/inflow-rust/settings/environments)
must allow deployments from `main` only. No permanent registry secret is required.

## Complete the release and publish later versions

After first publication and publisher configuration, run **Release** from the same
`main` commit with **publish** checked. Existing matching crates are skipped; the
workflow verifies a registry consumer, attests the archives, and creates `vVERSION`
and a GitHub release with the packages and verification reports attached.

Later versions use that workflow directly. The authentication action supplies a
short-lived token. Never move an existing version tag to another commit. If the
workflow fails after publication, rerun against the same commit; do not bump the
version merely to recover a missing GitHub release. An already completed GitHub
release is not overwritten.

Packaged-consumer checks extract the prepared archives and patch only the nine
unpublished SDK crates into a separate Cargo project. Registry-consumer checks use
exact crates.io versions without those patches. Neither mode sends payments.
