#!/bin/bash
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

IMAGE_NAME="starkbot"
CONTAINER_NAME="starkbot"

# Extract version from Cargo.toml
VERSION=$(grep '^version' stark-backend/Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')

usage() {
    echo "Usage: $0 [build|run|daemon|up|down|logs|shell|restart|status]"
    echo ""
    echo "  build    Build the Docker image"
    echo "  run      Build + run (foreground, Ctrl+C to stop)"
    echo "  daemon   Build + run in background (detached)"
    echo "  up       Start existing container (foreground)"
    echo "  down     Stop and remove container"
    echo "  logs     Tail container logs"
    echo "  shell    Open a shell inside the running container"
    echo "  restart  Restart the container"
    echo "  status   Show container status"
    echo ""
    echo "First time? Copy .env.template to .env and fill in your keys, then:"
    echo "  ./docker_run.sh run"
}

check_env() {
    if [ ! -f .env ]; then
        echo "ERROR: .env file not found!"
        echo "Copy the template and fill in your values:"
        echo "  cp .env.template .env"
        echo "  nano .env"
        exit 1
    fi
}

cmd_build() {
    echo "Building starkbot Docker image (v$VERSION)..."
    docker build $NO_CACHE \
        --build-arg STARKBOT_VERSION="$VERSION" \
        -t "$IMAGE_NAME:latest" \
        -t "$IMAGE_NAME:$VERSION" \
        .
    echo "Build complete: $IMAGE_NAME:$VERSION"
}

cmd_run() {
    check_env
    echo "Building and starting starkbot (v$VERSION)..."
    echo "Starkbot will be available at http://localhost:8080"
    echo "Press Ctrl+C to stop."
    echo ""
    docker compose build $NO_CACHE
    docker compose up
}

cmd_daemon() {
    check_env
    echo "Building and starting starkbot (v$VERSION) in background..."
    docker compose build $NO_CACHE
    docker compose up -d
    echo ""
    echo "Starkbot is running!"
    echo "  Web UI:  http://localhost:8080"
    echo "  Logs:    ./docker_run.sh logs"
    echo "  Stop:    ./docker_run.sh down"
}

cmd_up() {
    check_env
    echo "Starkbot will be available at http://localhost:8080"
    echo "Press Ctrl+C to stop."
    echo ""
    docker compose up
}

cmd_down() {
    docker compose down
    echo "Starkbot stopped."
}

cmd_logs() {
    docker compose logs -f --tail=100
}

cmd_shell() {
    docker compose exec backend bash
}

cmd_restart() {
    docker compose restart
    echo "Starkbot restarted."
}

cmd_status() {
    docker compose ps
}

# Parse flags
NO_CACHE=""
for arg in "$@"; do
    case "$arg" in
        --no-cache) NO_CACHE="--no-cache" ;;
    esac
done

# Default to "run" if no argument
CMD="${1:-}"

case "$CMD" in
    build)   cmd_build ;;
    run)     cmd_run ;;
    daemon)  cmd_daemon ;;
    up)      cmd_up ;;
    down)    cmd_down ;;
    logs)    cmd_logs ;;
    shell)   cmd_shell ;;
    restart) cmd_restart ;;
    status)  cmd_status ;;
    -h|--help|help) usage ;;
    "")      cmd_run ;;
    *)       echo "Unknown command: $CMD"; usage; exit 1 ;;
esac
