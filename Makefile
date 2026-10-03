.PHONY: coverage dependency docs format format-check lint package test tooling-test verify

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
	cargo package --workspace --exclude inflow-examples --exclude inflow-conformance --allow-dirty --locked

test:
	cargo test --workspace --all-features --locked

tooling-test:
	node --test scripts/*.test.mjs

verify: format-check lint test docs package dependency coverage tooling-test
