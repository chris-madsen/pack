.PHONY: help fmt fmt-check check test release benchmark clean

help:
	@printf '%s\n' \
		'make fmt        Format Rust sources' \
		'make fmt-check  Verify formatting' \
		'make check      Compile all targets' \
		'make test       Run the complete test suite' \
		'make release    Build the optimized CLI' \
		'make benchmark  Benchmark tests/noise with the release CLI' \
		'make clean      Remove Cargo build artifacts'

fmt:
	cargo fmt

fmt-check:
	cargo fmt -- --check

check: fmt-check
	cargo check --all-targets

test:
	cargo test

release:
	cargo build --release

benchmark: release
	target/release/pack benchmark tests/noise

clean:
	cargo clean
