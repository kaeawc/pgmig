# Plain cargo underneath.
.PHONY: check test fidelity bless lint fmt deny ci build

check:            ## type-check everything (no codegen, no link)
	cargo check --all-targets

test:             ## all tests against pgmem (nextest if installed)
	@command -v cargo-nextest >/dev/null && cargo nextest run --workspace || cargo test --workspace

fidelity:         ## all tests against real Postgres (set PGMIG_TEST_DSN)
	@test -n "$$PGMIG_TEST_DSN" || (echo "set PGMIG_TEST_DSN, e.g. host=127.0.0.1 port=5432 user=postgres" && exit 2)
	@command -v cargo-nextest >/dev/null && cargo nextest run --workspace || cargo test --workspace

bless:            ## rewrite corpus plan.sql files from the current planner output
	PGMIG_BLESS=1 cargo test -p pgmig --test it corpus

lint:
	cargo clippy --workspace --all-targets -- -D warnings

fmt:
	cargo fmt --all

deny:             ## dependency policy: duplicates, advisories, licenses, sources
	cargo deny check

ci: fmt lint deny test

build:
	cargo build --release -p pgmig-cli
