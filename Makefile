.PHONY: help build test test-db lint fmt check openapi dev-up dev-down dev-logs prod-up prod-down prod-logs

DEV  := docker compose -f docker-compose.dev.yml
PROD := docker compose -f docker-compose.prod.yml

help: ## List the targets
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-10s %s\n", $$1, $$2}'

build: ## Release build
	cargo build --release --locked

test: ## Unit tests
	cargo test --workspace

test-db: ## Unit + database tests (TEST_DATABASE_URL must point at a scratch database)
	@test -n "$(TEST_DATABASE_URL)" || (echo "set TEST_DATABASE_URL" && exit 1)
	cargo test --workspace

lint: ## Formatting and clippy, as in CI
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings

fmt: ## Format the code
	cargo fmt --all

check: lint test ## Everything CI runs on the Rust code

openapi: ## Write openapi.json (no database needed)
	cargo run --quiet -- openapi > openapi.json

dev-up: ## Start the development stack
	$(DEV) up -d --build

dev-down: ## Stop the development stack
	$(DEV) down

dev-logs: ## Follow the development logs
	$(DEV) logs -f router nginx

prod-up: ## Build and start the production stack
	$(PROD) up -d --build

prod-down: ## Stop the production stack
	$(PROD) down

prod-logs: ## Follow the production logs
	$(PROD) logs -f router nginx
