.PHONY: test dashboard docker-build docker-run clean

# Run comprehensive test suite
test:
	python3 tests/run_tests.py

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
