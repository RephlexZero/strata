.PHONY: up down logs clean build ps web-dev web-up

COMPOSE = docker compose -f ../docker-compose.yml

# Build images and start all services
up:
	$(COMPOSE) up --build -d

# Build images only
build:
	$(COMPOSE) build

# Start web dev hot-reload servers (dashboard on :8080, portal on :8081)
# First run builds images; subsequent runs reuse cached layers.
web-dev:
	$(COMPOSE) --profile web-dev up --build dashboard-dev portal-dev

# Restart web dev servers without rebuilding (fast, reuses existing images)
web-up:
	$(COMPOSE) --profile web-dev up dashboard-dev portal-dev

# Stop services (preserves data volume)
down:
	$(COMPOSE) down

# Tail logs for all services
logs:
	$(COMPOSE) logs -f

# Stop and delete everything including data
clean:
	$(COMPOSE) down -v --remove-orphans

# Show running containers
ps:
	$(COMPOSE) ps
