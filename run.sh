#!/usr/bin/env bash
# =============================================================================
# Evo System — Startup Script
# =============================================================================
#
# Starts the complete evo multi-agent system from ~/.evo-agents/.
#
# Usage:
#   ~/.evo-agents/run.sh              Start the system
#   ~/.evo-agents/run.sh --stop       Stop all running evo processes
#   ~/.evo-agents/run.sh --status     Show status of evo processes
#   ~/.evo-agents/run.sh --help       Show this help
#
# Startup order:
#   1. evo-gateway (background, port 3301)
#   2. Wait for gateway health check
#   3. evo-king (foreground, port 3300 — auto-discovers and spawns kernel agents)
#
# Environment:
#   EVO_HOME               Installation directory (default: ~/.evo-agents)
#   EVO_KING_PORT          King server port (default: 3300)
#   EVO_GATEWAY_PORT       Gateway server port (default: 3301)
#   OPENAI_API_KEY         OpenAI API key (passed to gateway)
#   ANTHROPIC_API_KEY      Anthropic API key (passed to gateway)
#   RUST_LOG               Log level filter (default: info)
#
# =============================================================================
set -euo pipefail

# -----------------------------------------------------------------------------
# Constants
# -----------------------------------------------------------------------------

EVO_HOME="${EVO_HOME:-${HOME}/.evo-agents}"
EVO_KING_PORT="${EVO_KING_PORT:-3300}"
EVO_GATEWAY_PORT="${EVO_GATEWAY_PORT:-3301}"

PIDFILE_GATEWAY="${EVO_HOME}/data/gateway.pid"
PIDFILE_KING="${EVO_HOME}/data/king.pid"

HEALTH_TIMEOUT=30        # seconds to wait for gateway health
SHUTDOWN_TIMEOUT=10       # seconds to wait for graceful shutdown

# -----------------------------------------------------------------------------
# Color output helpers
# -----------------------------------------------------------------------------

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
BOLD='\033[1m'
DIM='\033[2m'
NC='\033[0m'

info()    { printf "${BLUE}[evo]${NC} %s\n" "$*"; }
success() { printf "${GREEN}[evo]${NC} %s\n" "$*"; }
warn()    { printf "${YELLOW}[evo]${NC} %s\n" "$*"; }
error()   { printf "${RED}[evo]${NC} %s\n" "$*" >&2; }
step()    { printf "${CYAN}[evo]${NC} ${BOLD}%s${NC}\n" "$*"; }

# -----------------------------------------------------------------------------
# PID file management
# -----------------------------------------------------------------------------

write_pid() {
    local pidfile="$1"
    local pid="$2"
    echo "${pid}" > "${pidfile}"
}

read_pid() {
    local pidfile="$1"
    if [[ -f "${pidfile}" ]]; then
        cat "${pidfile}"
    else
        echo ""
    fi
}

remove_pid() {
    local pidfile="$1"
    rm -f "${pidfile}"
}

# Check if a process is running by PID
is_running() {
    local pid="$1"
    [[ -n "${pid}" ]] && kill -0 "${pid}" 2>/dev/null
}

# -----------------------------------------------------------------------------
# Graceful shutdown: send SIGTERM, wait, then SIGKILL if needed
# -----------------------------------------------------------------------------

graceful_kill() {
    local pid="$1"
    local name="$2"
    local timeout="${3:-${SHUTDOWN_TIMEOUT}}"

    if ! is_running "${pid}"; then
        return 0
    fi

    info "Stopping ${name} (PID: ${pid})..."
    kill "${pid}" 2>/dev/null || true

    local elapsed=0
    while is_running "${pid}" && [[ ${elapsed} -lt ${timeout} ]]; do
        sleep 1
        elapsed=$((elapsed + 1))
    done

    if is_running "${pid}"; then
        warn "${name} did not stop gracefully, sending SIGKILL..."
        kill -9 "${pid}" 2>/dev/null || true
        sleep 1
    fi

    if is_running "${pid}"; then
        error "Failed to stop ${name} (PID: ${pid})"
        return 1
    fi

    success "${name} stopped"
    return 0
}

# -----------------------------------------------------------------------------
# Cleanup trap: kill gateway when king exits
# -----------------------------------------------------------------------------

GATEWAY_PID=""

cleanup() {
    local exit_code=$?

    # Remove king PID file
    remove_pid "${PIDFILE_KING}"

    # Stop the gateway background process
    if [[ -n "${GATEWAY_PID}" ]] && is_running "${GATEWAY_PID}"; then
        info "King exited — shutting down gateway..."
        graceful_kill "${GATEWAY_PID}" "evo-gateway"
        remove_pid "${PIDFILE_GATEWAY}"
    fi

    if [[ ${exit_code} -eq 0 ]]; then
        success "Evo system shut down cleanly"
    else
        warn "Evo system exited with code ${exit_code}"
    fi
}

# -----------------------------------------------------------------------------
# --stop: find and kill running king + gateway processes
# -----------------------------------------------------------------------------

do_stop() {
    step "Stopping evo system..."

    local stopped=0

    # Stop king first (it manages agents)
    local king_pid
    king_pid="$(read_pid "${PIDFILE_KING}")"
    if [[ -n "${king_pid}" ]] && is_running "${king_pid}"; then
        graceful_kill "${king_pid}" "evo-king"
        remove_pid "${PIDFILE_KING}"
        stopped=$((stopped + 1))
    else
        info "evo-king is not running (no PID file or stale PID)"
        remove_pid "${PIDFILE_KING}"
    fi

    # Stop gateway
    local gw_pid
    gw_pid="$(read_pid "${PIDFILE_GATEWAY}")"
    if [[ -n "${gw_pid}" ]] && is_running "${gw_pid}"; then
        graceful_kill "${gw_pid}" "evo-gateway"
        remove_pid "${PIDFILE_GATEWAY}"
        stopped=$((stopped + 1))
    else
        info "evo-gateway is not running (no PID file or stale PID)"
        remove_pid "${PIDFILE_GATEWAY}"
    fi

    # Also attempt to find processes by name as a fallback
    local fallback_pids
    fallback_pids="$(pgrep -f "${EVO_HOME}/bin/evo-" 2>/dev/null || true)"
    if [[ -n "${fallback_pids}" ]]; then
        warn "Found additional evo processes, stopping them..."
        echo "${fallback_pids}" | while read -r pid; do
            graceful_kill "${pid}" "evo-process"
            stopped=$((stopped + 1))
        done
    fi

    if [[ ${stopped} -eq 0 ]]; then
        info "No running evo processes found"
    else
        success "Stopped ${stopped} process(es)"
    fi
}

# -----------------------------------------------------------------------------
# --status: show running evo processes
# -----------------------------------------------------------------------------

do_status() {
    step "Evo system status"
    printf "\n"

    local any_running=false

    # Gateway
    local gw_pid
    gw_pid="$(read_pid "${PIDFILE_GATEWAY}")"
    if [[ -n "${gw_pid}" ]] && is_running "${gw_pid}"; then
        printf "  ${GREEN}running${NC}  evo-gateway   PID %-8s  port %s\n" "${gw_pid}" "${EVO_GATEWAY_PORT}"
        any_running=true
    else
        printf "  ${DIM}stopped${NC}  evo-gateway\n"
    fi

    # King
    local king_pid
    king_pid="$(read_pid "${PIDFILE_KING}")"
    if [[ -n "${king_pid}" ]] && is_running "${king_pid}"; then
        printf "  ${GREEN}running${NC}  evo-king      PID %-8s  port %s\n" "${king_pid}" "${EVO_KING_PORT}"
        any_running=true
    else
        printf "  ${DIM}stopped${NC}  evo-king\n"
    fi

    # Any other evo processes
    local other_pids
    other_pids="$(pgrep -f "${EVO_HOME}/evo-kernel-agent-" 2>/dev/null || true)"
    if [[ -n "${other_pids}" ]]; then
        local count
        count="$(echo "${other_pids}" | wc -l | tr -d ' ')"
        printf "  ${GREEN}running${NC}  kernel agents  (%s processes)\n" "${count}"
        any_running=true
    else
        printf "  ${DIM}stopped${NC}  kernel agents\n"
    fi

    printf "\n"

    if [[ "${any_running}" == "true" ]]; then
        info "System is running"
    else
        info "System is stopped"
    fi
}

# -----------------------------------------------------------------------------
# --help
# -----------------------------------------------------------------------------

do_help() {
    printf "${BOLD}Evo System — Startup Script${NC}\n\n"
    printf "Usage:\n"
    printf "  %s              Start the evo system\n" "$0"
    printf "  %s --stop       Stop all running evo processes\n" "$0"
    printf "  %s --status     Show status of evo processes\n" "$0"
    printf "  %s --help       Show this help\n\n" "$0"
    printf "Environment:\n"
    printf "  EVO_HOME=%s\n" "${EVO_HOME}"
    printf "  EVO_KING_PORT=%s\n" "${EVO_KING_PORT}"
    printf "  EVO_GATEWAY_PORT=%s\n\n" "${EVO_GATEWAY_PORT}"
}

# -----------------------------------------------------------------------------
# Verify installation
# -----------------------------------------------------------------------------

verify_installation() {
    if [[ ! -d "${EVO_HOME}" ]]; then
        error "${EVO_HOME} not found. Run install.sh first."
        error "  curl -fsSL https://raw.githubusercontent.com/ai-evo-agents/evo-king/main/install.sh | bash"
        exit 1
    fi

    if [[ ! -x "${EVO_HOME}/bin/evo-gateway" ]]; then
        error "evo-gateway binary not found at ${EVO_HOME}/bin/evo-gateway"
        error "Re-run install.sh to download it."
        exit 1
    fi

    if [[ ! -x "${EVO_HOME}/bin/evo-king" ]]; then
        error "evo-king binary not found at ${EVO_HOME}/bin/evo-king"
        error "Re-run install.sh to download it."
        exit 1
    fi
}

# -----------------------------------------------------------------------------
# Check for port conflicts
# -----------------------------------------------------------------------------

check_port() {
    local port="$1"
    local name="$2"

    # Use lsof on macOS/Linux to check port availability
    if command -v lsof &>/dev/null; then
        if lsof -iTCP:"${port}" -sTCP:LISTEN -P -n &>/dev/null; then
            error "Port ${port} is already in use (needed by ${name})"
            error "Stop the conflicting process or set a different port:"
            if [[ "${name}" == "evo-gateway" ]]; then
                error "  EVO_GATEWAY_PORT=9090 ${0}"
            else
                error "  EVO_KING_PORT=4000 ${0}"
            fi
            exit 1
        fi
    fi
}

# -----------------------------------------------------------------------------
# Start the system
# -----------------------------------------------------------------------------

do_start() {
    printf "\n${BOLD}${CYAN}Evo System${NC}\n\n"

    # --- Pre-flight checks ---------------------------------------------------

    verify_installation

    # Check for already-running processes
    local existing_king
    existing_king="$(read_pid "${PIDFILE_KING}")"
    if [[ -n "${existing_king}" ]] && is_running "${existing_king}"; then
        error "evo-king is already running (PID: ${existing_king})"
        error "Use '${0} --stop' to stop it first, or '${0} --status' to check."
        exit 1
    fi

    check_port "${EVO_GATEWAY_PORT}" "evo-gateway"
    check_port "${EVO_KING_PORT}" "evo-king"

    # --- Set up cleanup trap -------------------------------------------------

    trap cleanup EXIT INT TERM

    # --- Start gateway in background -----------------------------------------

    step "Starting evo-gateway on port ${EVO_GATEWAY_PORT}..."

    GATEWAY_CONFIG="${EVO_HOME}/data/gateway.json" \
    EVO_GATEWAY_DB_PATH="${EVO_HOME}/data/gateway.db" \
    EVO_LOG_DIR="${EVO_HOME}/logs" \
    RUST_LOG="${RUST_LOG:-info}" \
    "${EVO_HOME}/bin/evo-gateway" &
    GATEWAY_PID=$!

    write_pid "${PIDFILE_GATEWAY}" "${GATEWAY_PID}"
    info "Gateway started (PID: ${GATEWAY_PID})"

    # --- Wait for gateway health check ---------------------------------------

    step "Waiting for gateway health check..."

    local healthy=false
    for i in $(seq 1 "${HEALTH_TIMEOUT}"); do
        if curl -sf "http://localhost:${EVO_GATEWAY_PORT}/health" > /dev/null 2>&1; then
            healthy=true
            break
        fi

        # Check if gateway process died
        if ! is_running "${GATEWAY_PID}"; then
            error "Gateway process exited unexpectedly"
            error "Check logs: ${EVO_HOME}/logs/"
            exit 1
        fi

        # Print progress dots every 5 seconds
        if (( i % 5 == 0 )); then
            info "Still waiting... (${i}/${HEALTH_TIMEOUT}s)"
        fi
        sleep 1
    done

    if [[ "${healthy}" == "true" ]]; then
        success "Gateway is healthy"
    else
        warn "Gateway health check timed out after ${HEALTH_TIMEOUT}s"
        warn "Continuing anyway — king will retry connections"
    fi

    # --- Start king in foreground --------------------------------------------

    step "Starting evo-king on port ${EVO_KING_PORT}..."
    info "King will auto-discover and spawn kernel agents"
    printf "\n"

    write_pid "${PIDFILE_KING}" "$$"

    # King runs in the foreground. When it exits, the cleanup trap fires.
    KERNEL_AGENTS_DIR="${EVO_HOME}" \
    EVO_KING_PORT="${EVO_KING_PORT}" \
    EVO_KING_DB_PATH="${EVO_HOME}/data/king.db" \
    EVO_DASHBOARD_DIR="${EVO_HOME}/dashboard" \
    EVO_LOG_DIR="${EVO_HOME}/logs" \
    EVO_GATEWAY_CONFIG="${EVO_HOME}/data/gateway.json" \
    EVO_GATEWAY_RUNNING_CONFIG="${EVO_HOME}/data/gateway-running.conf" \
    EVO_GATEWAY_BACKUP_DIR="${EVO_HOME}/data/backups" \
    RUNNER_BINARY="${EVO_HOME}/bin/evo-runner" \
    RUST_LOG="${RUST_LOG:-info}" \
    "${EVO_HOME}/bin/evo-king"
}

# =============================================================================
# Main — parse arguments
# =============================================================================

main() {
    case "${1:-}" in
        --stop|-s)
            do_stop
            ;;
        --status)
            do_status
            ;;
        --help|-h)
            do_help
            ;;
        "")
            do_start
            ;;
        *)
            error "Unknown option: $1"
            do_help
            exit 1
            ;;
    esac
}

main "$@"
