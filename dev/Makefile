.PHONY: up down logs clean build ps dashboard

# Build images and start all services
up: build
	docker compose up -d

# Build images only
build:
	docker compose build

# Build the dashboard WASM bundle
dashboard:
	cd ../crates/strata-dashboard && trunk build --release

# Stop services (preserves data volume)
down:
	docker compose down

# Tail logs for all services
logs:
	docker compose logs -f

# Stop and delete everything including data
clean:
	docker compose down -v --remove-orphans

# Show running containers
ps:
	docker compose ps
