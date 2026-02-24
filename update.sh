#!/usr/bin/env bash
# =============================================================================
# Evo System — Component Update Script
# =============================================================================
#
# Updates a single evo component (king, gateway, or kernel agent) to a
# specified version. Designed to be called by king's update pipeline agent
# after a new release has been validated.
#
# Usage:
#   update.sh <component> <new-version> [--keep-backup]
#
# Examples:
#   update.sh evo-kernel-agent-learning v0.2.1
#   update.sh evo-king v0.2.1 --keep-backup
#   update.sh evo-gateway v0.3.0
#
# Components:
#   Services:      evo-king, evo-gateway
#   Kernel agents: evo-kernel-agent-learning, evo-kernel-agent-building,
#                  evo-kernel-agent-pre-load, evo-kernel-agent-evaluation,
#                  evo-kernel-agent-skill-manage, evo-kernel-agent-update
#
# What this script does:
#   1. Detects platform (Linux x86_64/ARM64, macOS Intel/Apple Silicon)
#   2. Backs up the current version
#   3. Downloads the new version from GitHub Releases
#   4. Extracts and installs
#   5. Updates repos.json with the new version
#   6. Verifies the new binary is runnable
#   7. Rolls back from backup if verification fails
#   8. Cleans up backup on success (unless --keep-backup)
#
# Environment:
#   EVO_HOME      Installation directory (default: ~/.evo-agents)
#   GITHUB_TOKEN  Optional GitHub token for higher API rate limits
#
# Exit codes:
#   0 — update successful
#   1 — update failed (rolled back to previous version)
#   2 — invalid arguments or missing component
#
# =============================================================================
set -euo pipefail

# -----------------------------------------------------------------------------
# Constants
# -----------------------------------------------------------------------------

GITHUB_ORG="ai-evo-agents"
EVO_HOME="${EVO_HOME:-${HOME}/.evo-agents}"

# Map: component name -> binary name (for agents with different binary names)
declare -A BINARY_MAP=(
    [evo-kernel-agent-learning]="evo-agent-learning"
    [evo-kernel-agent-building]="evo-agent-building"
    [evo-kernel-agent-pre-load]="evo-agent-pre-load"
    [evo-kernel-agent-evaluation]="evo-agent-evaluation"
    [evo-kernel-agent-skill-manage]="evo-agent-skill-manage"
    [evo-kernel-agent-update]="evo-agent-update"
    [evo-king]="evo-king"
    [evo-gateway]="evo-gateway"
)

# Components classified by type
SERVICE_COMPONENTS="evo-king evo-gateway"
AGENT_COMPONENTS="evo-kernel-agent-learning evo-kernel-agent-building evo-kernel-agent-pre-load evo-kernel-agent-evaluation evo-kernel-agent-skill-manage evo-kernel-agent-update"

# -----------------------------------------------------------------------------
# Color output helpers
# -----------------------------------------------------------------------------

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

info()    { printf "${BLUE}[update]${NC}  %s\n" "$*"; }
success() { printf "${GREEN}[update]${NC}  %s\n" "$*"; }
warn()    { printf "${YELLOW}[update]${NC}  %s\n" "$*"; }
error()   { printf "${RED}[update]${NC}  %s\n" "$*" >&2; }
step()    { printf "${CYAN}[update]${NC}  ${BOLD}%s${NC}\n" "$*"; }

# -----------------------------------------------------------------------------
# Usage
# -----------------------------------------------------------------------------

usage() {
    printf "${BOLD}Evo System — Component Update${NC}\n\n"
    printf "Usage:\n"
    printf "  %s <component> <version> [--keep-backup]\n\n" "$0"
    printf "Components:\n"
    printf "  evo-king                         Central orchestrator\n"
    printf "  evo-gateway                      API gateway\n"
    printf "  evo-kernel-agent-learning        Skill discovery agent\n"
    printf "  evo-kernel-agent-building        Skill packaging agent\n"
    printf "  evo-kernel-agent-pre-load        Health check agent\n"
    printf "  evo-kernel-agent-evaluation      Skill scoring agent\n"
    printf "  evo-kernel-agent-skill-manage    Skill lifecycle agent\n"
    printf "  evo-kernel-agent-update          Self-update agent\n\n"
    printf "Options:\n"
    printf "  --keep-backup    Keep backup files after successful update\n\n"
    printf "Examples:\n"
    printf "  %s evo-kernel-agent-learning v0.2.1\n" "$0"
    printf "  %s evo-king v0.2.1 --keep-backup\n\n" "$0"
}

# -----------------------------------------------------------------------------
# Platform detection (same logic as install.sh)
# -----------------------------------------------------------------------------

detect_platform() {
    local os arch

    os="$(uname -s)"
    arch="$(uname -m)"

    case "${os}" in
        Linux)  os="unknown-linux-gnu" ;;
        Darwin) os="apple-darwin" ;;
        *)
            error "Unsupported operating system: ${os}"
            exit 2
            ;;
    esac

    case "${arch}" in
        x86_64)          arch="x86_64" ;;
        aarch64|arm64)   arch="aarch64" ;;
        *)
            error "Unsupported architecture: ${arch}"
            exit 2
            ;;
    esac

    echo "${arch}-${os}"
}

# -----------------------------------------------------------------------------
# Determine component type: "service" or "kernel-agent"
# -----------------------------------------------------------------------------

get_component_type() {
    local component="$1"

    case " ${SERVICE_COMPONENTS} " in
        *" ${component} "*) echo "service" ;;
        *)
            case " ${AGENT_COMPONENTS} " in
                *" ${component} "*) echo "kernel-agent" ;;
                *)
                    error "Unknown component: ${component}"
                    printf "\nValid components:\n"
                    printf "  %s\n" ${SERVICE_COMPONENTS} ${AGENT_COMPONENTS}
                    exit 2
                    ;;
            esac
            ;;
    esac
}

# -----------------------------------------------------------------------------
# HTTP helper
# -----------------------------------------------------------------------------

download_file() {
    local url="$1"
    local dest="$2"
    local -a headers=()

    if [[ -n "${GITHUB_TOKEN:-}" ]]; then
        headers+=(-H "Authorization: Bearer ${GITHUB_TOKEN}")
    fi
    headers+=(-H "Accept: application/octet-stream")

    curl -fsSL "${headers[@]}" -o "${dest}" "${url}"
}

# -----------------------------------------------------------------------------
# Backup current version
# -----------------------------------------------------------------------------

backup_service() {
    local component="$1"
    local binary_path="${EVO_HOME}/bin/${component}"
    local backup_path="${binary_path}.backup"

    if [[ ! -f "${binary_path}" ]]; then
        warn "No existing binary at ${binary_path} — nothing to back up"
        return 0
    fi

    info "Backing up ${binary_path} -> ${backup_path}"
    cp "${binary_path}" "${backup_path}"
    success "Backup created"
}

backup_agent() {
    local component="$1"
    local agent_dir="${EVO_HOME}/${component}"
    local backup_dir="${agent_dir}.backup"

    if [[ ! -d "${agent_dir}" ]]; then
        warn "No existing directory at ${agent_dir} — nothing to back up"
        return 0
    fi

    # Remove stale backup if present
    if [[ -d "${backup_dir}" ]]; then
        warn "Removing stale backup at ${backup_dir}"
        rm -rf "${backup_dir}"
    fi

    info "Backing up ${agent_dir} -> ${backup_dir}"
    cp -r "${agent_dir}" "${backup_dir}"
    success "Backup created"
}

# -----------------------------------------------------------------------------
# Rollback from backup
# -----------------------------------------------------------------------------

rollback_service() {
    local component="$1"
    local binary_path="${EVO_HOME}/bin/${component}"
    local backup_path="${binary_path}.backup"

    if [[ -f "${backup_path}" ]]; then
        warn "Rolling back ${component} from backup..."
        mv "${backup_path}" "${binary_path}"
        chmod +x "${binary_path}"
        success "Rollback complete — previous version restored"
    else
        error "No backup found at ${backup_path} — cannot rollback"
    fi
}

rollback_agent() {
    local component="$1"
    local agent_dir="${EVO_HOME}/${component}"
    local backup_dir="${agent_dir}.backup"

    if [[ -d "${backup_dir}" ]]; then
        warn "Rolling back ${component} from backup..."
        rm -rf "${agent_dir}"
        mv "${backup_dir}" "${agent_dir}"
        success "Rollback complete — previous version restored"
    else
        error "No backup found at ${backup_dir} — cannot rollback"
    fi
}

# -----------------------------------------------------------------------------
# Download and extract new version
# -----------------------------------------------------------------------------

download_and_install_service() {
    local component="$1"
    local binary_name="$2"
    local version="$3"
    local target="$4"

    local archive_name="${binary_name}-${version}-${target}.tar.gz"
    local download_url="https://github.com/${GITHUB_ORG}/${component}/releases/download/${version}/${archive_name}"

    local tmp_archive
    tmp_archive="$(mktemp "${TMPDIR:-/tmp}/evo-update-XXXXXX.tar.gz")"

    local tmp_extract
    tmp_extract="$(mktemp -d "${TMPDIR:-/tmp}/evo-update-XXXXXX")"

    info "Downloading ${archive_name}..."
    if ! download_file "${download_url}" "${tmp_archive}"; then
        rm -f "${tmp_archive}"
        rm -rf "${tmp_extract}"
        error "Download failed: ${download_url}"
        return 1
    fi

    info "Extracting..."
    tar -xzf "${tmp_archive}" -C "${tmp_extract}"
    rm -f "${tmp_archive}"

    # Flat archive: binary at archive root
    if [[ -f "${tmp_extract}/${binary_name}" ]]; then
        mv "${tmp_extract}/${binary_name}" "${EVO_HOME}/bin/${binary_name}"
        chmod +x "${EVO_HOME}/bin/${binary_name}"
        success "Installed ${binary_name} -> ${EVO_HOME}/bin/${binary_name}"
    else
        rm -rf "${tmp_extract}"
        error "Binary '${binary_name}' not found in archive"
        return 1
    fi

    rm -rf "${tmp_extract}"
    return 0
}

download_and_install_agent() {
    local component="$1"
    local binary_name="$2"
    local version="$3"
    local target="$4"

    local archive_name="${binary_name}-${version}-${target}.tar.gz"
    local download_url="https://github.com/${GITHUB_ORG}/${component}/releases/download/${version}/${archive_name}"

    local tmp_archive
    tmp_archive="$(mktemp "${TMPDIR:-/tmp}/evo-update-XXXXXX.tar.gz")"

    info "Downloading ${archive_name}..."
    if ! download_file "${download_url}" "${tmp_archive}"; then
        rm -f "${tmp_archive}"
        error "Download failed: ${download_url}"
        return 1
    fi

    # Remove old agent directory (backup already exists)
    local agent_dir="${EVO_HOME}/${component}"
    if [[ -d "${agent_dir}" ]]; then
        rm -rf "${agent_dir}"
    fi

    info "Extracting into ${EVO_HOME}/..."
    tar -xzf "${tmp_archive}" -C "${EVO_HOME}"
    rm -f "${tmp_archive}"

    # Ensure binary is executable
    if [[ -f "${agent_dir}/${binary_name}" ]]; then
        chmod +x "${agent_dir}/${binary_name}"
        success "Installed ${component} -> ${agent_dir}/"
    else
        error "Expected ${agent_dir}/${binary_name} not found after extraction"
        return 1
    fi

    return 0
}

# -----------------------------------------------------------------------------
# Update repos.json with new version
# -----------------------------------------------------------------------------

update_repos_json() {
    local component="$1"
    local version="$2"
    local repos_json="${EVO_HOME}/repos.json"

    if [[ ! -f "${repos_json}" ]]; then
        warn "repos.json not found at ${repos_json} — skipping version update"
        return 0
    fi

    info "Updating repos.json for ${component} -> ${version}"

    # Use jq if available for clean JSON manipulation
    if command -v jq &>/dev/null; then
        local tmp_json
        tmp_json="$(mktemp "${TMPDIR:-/tmp}/evo-json-XXXXXX.json")"

        jq --arg comp "${component}" --arg ver "${version}" \
            '.repos[$comp].installed_version = $ver' \
            "${repos_json}" > "${tmp_json}"

        mv "${tmp_json}" "${repos_json}"
        success "repos.json updated (via jq)"
    else
        # Fallback: use sed for in-place replacement
        # This targets the "installed_version" line within the component block.
        # We find the component key, then update the next installed_version line.
        local tmp_json
        tmp_json="$(mktemp "${TMPDIR:-/tmp}/evo-json-XXXXXX.json")"

        # Use awk for a more reliable multi-line edit
        awk -v comp="\"${component}\"" -v ver="\"${version}\"" '
        {
            if ($0 ~ comp ":") {
                found = 1
            }
            if (found && $0 ~ /"installed_version"/) {
                sub(/"installed_version":.*/, "\"installed_version\": " ver ",")
                found = 0
            }
            print
        }
        ' "${repos_json}" > "${tmp_json}"

        mv "${tmp_json}" "${repos_json}"
        success "repos.json updated (via awk)"
    fi
}

# -----------------------------------------------------------------------------
# Verify new binary works
# -----------------------------------------------------------------------------

verify_binary() {
    local binary_path="$1"
    local component="$2"

    info "Verifying ${component}..."

    if [[ ! -f "${binary_path}" ]]; then
        error "Binary not found: ${binary_path}"
        return 1
    fi

    if [[ ! -x "${binary_path}" ]]; then
        error "Binary not executable: ${binary_path}"
        return 1
    fi

    # Try running with --version or --help to verify it is a valid binary.
    # Some binaries may not support these flags, so we also accept a quick
    # execution that exits with code 0 or prints anything to stdout/stderr.
    # We use a short timeout to avoid hanging on binaries that start a server.

    local verify_ok=false

    # Method 1: Try --version
    if timeout 5 "${binary_path}" --version &>/dev/null 2>&1; then
        verify_ok=true
    fi

    # Method 2: Try --help (some binaries only support this)
    if [[ "${verify_ok}" == "false" ]]; then
        if timeout 5 "${binary_path}" --help &>/dev/null 2>&1; then
            verify_ok=true
        fi
    fi

    # Method 3: Check if it is a valid ELF/Mach-O binary via file command
    if [[ "${verify_ok}" == "false" ]]; then
        if command -v file &>/dev/null; then
            local file_type
            file_type="$(file -b "${binary_path}" 2>/dev/null || true)"
            if [[ "${file_type}" == *"executable"* ]] || [[ "${file_type}" == *"Mach-O"* ]]; then
                verify_ok=true
                info "Binary appears valid (${file_type})"
            fi
        fi
    fi

    if [[ "${verify_ok}" == "true" ]]; then
        success "Verification passed for ${component}"
        return 0
    else
        error "Verification failed for ${component}"
        return 1
    fi
}

# -----------------------------------------------------------------------------
# Clean up backup files
# -----------------------------------------------------------------------------

cleanup_backup_service() {
    local component="$1"
    local backup_path="${EVO_HOME}/bin/${component}.backup"

    if [[ -f "${backup_path}" ]]; then
        info "Cleaning up backup: ${backup_path}"
        rm -f "${backup_path}"
    fi
}

cleanup_backup_agent() {
    local component="$1"
    local backup_dir="${EVO_HOME}/${component}.backup"

    if [[ -d "${backup_dir}" ]]; then
        info "Cleaning up backup: ${backup_dir}"
        rm -rf "${backup_dir}"
    fi
}

# =============================================================================
# Main
# =============================================================================

main() {
    # --- Argument parsing ----------------------------------------------------

    if [[ $# -lt 2 ]]; then
        usage
        exit 2
    fi

    local component="$1"
    local version="$2"
    local keep_backup=false

    # Parse optional flags
    shift 2
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --keep-backup)
                keep_backup=true
                shift
                ;;
            --help|-h)
                usage
                exit 0
                ;;
            *)
                error "Unknown option: $1"
                usage
                exit 2
                ;;
        esac
    done

    # --- Validate inputs -----------------------------------------------------

    local binary_name="${BINARY_MAP[${component}]:-}"
    if [[ -z "${binary_name}" ]]; then
        error "Unknown component: ${component}"
        usage
        exit 2
    fi

    local comp_type
    comp_type="$(get_component_type "${component}")"

    if [[ ! -d "${EVO_HOME}" ]]; then
        error "${EVO_HOME} not found. Run install.sh first."
        exit 1
    fi

    # --- Start update --------------------------------------------------------

    printf "\n${BOLD}${CYAN}Evo System — Updating ${component}${NC}\n\n"

    info "Component:  ${component}"
    info "Binary:     ${binary_name}"
    info "Version:    ${version}"
    info "Type:       ${comp_type}"

    local target
    target="$(detect_platform)"
    info "Platform:   ${target}"
    printf "\n"

    # --- Step 1: Backup current version --------------------------------------

    step "Step 1/5: Backing up current version"

    if [[ "${comp_type}" == "service" ]]; then
        backup_service "${component}"
    else
        backup_agent "${component}"
    fi

    # --- Step 2: Download and install new version ----------------------------

    step "Step 2/5: Downloading and installing ${version}"

    local install_ok=true
    if [[ "${comp_type}" == "service" ]]; then
        if ! download_and_install_service "${component}" "${binary_name}" "${version}" "${target}"; then
            install_ok=false
        fi
    else
        if ! download_and_install_agent "${component}" "${binary_name}" "${version}" "${target}"; then
            install_ok=false
        fi
    fi

    if [[ "${install_ok}" == "false" ]]; then
        error "Installation failed — rolling back"
        if [[ "${comp_type}" == "service" ]]; then
            rollback_service "${component}"
        else
            rollback_agent "${component}"
        fi
        exit 1
    fi

    # --- Step 3: Verify new binary -------------------------------------------

    step "Step 3/5: Verifying new binary"

    local binary_path
    if [[ "${comp_type}" == "service" ]]; then
        binary_path="${EVO_HOME}/bin/${binary_name}"
    else
        binary_path="${EVO_HOME}/${component}/${binary_name}"
    fi

    if ! verify_binary "${binary_path}" "${component}"; then
        error "Verification failed — rolling back to previous version"
        if [[ "${comp_type}" == "service" ]]; then
            rollback_service "${component}"
        else
            rollback_agent "${component}"
        fi
        exit 1
    fi

    # --- Step 4: Update repos.json -------------------------------------------

    step "Step 4/5: Updating repos.json"

    update_repos_json "${component}" "${version}"

    # --- Step 5: Clean up ----------------------------------------------------

    step "Step 5/5: Cleanup"

    if [[ "${keep_backup}" == "true" ]]; then
        info "Keeping backup (--keep-backup)"
    else
        if [[ "${comp_type}" == "service" ]]; then
            cleanup_backup_service "${component}"
        else
            cleanup_backup_agent "${component}"
        fi
        success "Backup cleaned up"
    fi

    # --- Done ----------------------------------------------------------------

    printf "\n"
    success "Successfully updated ${component} to ${version}"
    printf "\n"

    # If we just updated king or gateway, remind about restart
    if [[ "${comp_type}" == "service" ]]; then
        info "Note: Restart the evo system for changes to take effect:"
        info "  ${EVO_HOME}/run.sh --stop && ${EVO_HOME}/run.sh"
    fi

    printf "\n"
}

main "$@"
