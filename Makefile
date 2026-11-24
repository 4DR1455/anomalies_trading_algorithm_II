.PHONY: test rust-test rust-build dashboard docker-build docker-run clean

# Run comprehensive Python test suite
test:
	python3 tests/run_tests.py

# Run Rust unit & integration test suite with zero warnings via rust:alpine
rust-test:
	docker run --rm --security-opt label=disable -v $$(pwd):/workspace -v anomalies-cargo-cache:/usr/local/cargo/registry -w /workspace rust:alpine sh -c "apk add --no-cache musl-dev && RUSTFLAGS='-D warnings' cargo test"

# Build static Rust release binary with zero warnings via rust:alpine
rust-build:
	docker run --rm --security-opt label=disable -v $$(pwd):/workspace -v anomalies-cargo-cache:/usr/local/cargo/registry -w /workspace rust:alpine sh -c "apk add --no-cache musl-dev && RUSTFLAGS='-D warnings' cargo build --release"

# Generate and serve the local analytics dashboard on 127.0.0.1:8080
dashboard:
	python3 dashboard/serve_local.py

# Build hardened Docker container (non-root, no ports exposed)
docker-build:
	docker build -t anomalies-trading-engine:v2.0 .

# Run hardened container with mounted volumes for state and config
docker-run:
	mkdir -p data
	docker run -d \
		--name anomalies-bot \
		--restart unless-stopped \
		--read-only \
		--tmpfs /tmp \
		-v $$(pwd)/data:/data:rw \
		-v $$(pwd)/.env:/app/.env:ro \
		-v $$(pwd)/params.json:/app/params.json:ro \
		anomalies-trading-engine:v2.0

# Clean temporary files
clean:
	rm -rf data/*.tmp target/
