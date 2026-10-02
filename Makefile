.PHONY: coverage dependency docs format format-check lint package test verify

coverage:
	cargo llvm-cov --workspace --all-features --locked --ignore-filename-regex '/tests/' --fail-under-lines 99 --fail-under-file-lines 99 --fail-under-functions 99 --fail-under-regions 99 --lcov --output-path lcov.info

dependency:
	cargo deny check

docs:
	RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked

format:
	cargo fmt --all

format-check:
	cargo fmt --all --check

lint:
	cargo clippy --workspace --all-targets --all-features --locked -- -D warnings

package:
	@set -e; for manifest in crates/*/Cargo.toml; do cargo package --manifest-path "$$manifest" --allow-dirty --locked; done

test:
	cargo test --workspace --all-features --locked

verify: format-check lint test docs package dependency coverage
