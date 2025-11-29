build:
	cargo build --release --features standalone
test:
	cargo test --features standalone --features test --features client
clean:
	cargo clean