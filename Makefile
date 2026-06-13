.PHONY: help fmt fmt-check check test test-fast release benchmark benchmark-generator clean

help:
	@printf '%s\n' \
		'make fmt        Format Rust sources' \
		'make fmt-check  Verify formatting' \
		'make check      Compile all targets' \
		'make test       Run the complete test suite' \
		'make test-fast  Run fast kernel/unit tests only' \
		'make release    Build the optimized CLI' \
		'make benchmark  Benchmark tests/noise with the release CLI' \
		'make benchmark-generator  Benchmark tests/noise in strict generator-only mode' \
		'make clean      Remove Cargo build artifacts'

fmt:
	cargo fmt

fmt-check:
	cargo fmt -- --check

check: fmt-check
	cargo check --all-targets

test:
	cargo test

test-fast:
	cargo test domain::kernel:: --lib

release:
	cargo build --release

benchmark: release
	target/release/pack benchmark tests/noise

benchmark-generator: release
	target/release/pack benchmark-generator tests/noise

clean:
	cargo clean
