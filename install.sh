#!/usr/bin/env bash
# =============================================================================
# Evo System — Bootstrap Installer
# =============================================================================
#
# Installs the complete evo multi-agent system into ~/.evo-agents/.
#
# Quick install:
#   curl -fsSL https://raw.githubusercontent.com/ai-evo-agents/evo-king/main/install.sh | bash
#
# What this script does:
#   1. Detects platform (Linux x86_64/ARM64, macOS Intel/Apple Silicon)
#   2. Creates ~/.evo-agents/ directory structure
#   3. Downloads latest release binaries for king, gateway, and all kernel agents
#   4. Clones all source repos for the self-upgrade pipeline
#   5. Generates repos.json manifest
#   6. Copies run.sh and update.sh from the king release archive
#
# Requirements:
#   - bash 4+ (macOS ships bash 3 but this script is compatible)
#   - curl or wget
#   - tar
#   - git (for cloning repos)
#   - jq (optional — used for JSON generation, falls back to printf)
#
# Environment:
#   EVO_HOME    — installation directory (default: ~/.evo-agents)
#   GITHUB_TOKEN — optional GitHub token for higher API rate limits
#
# =============================================================================
set -euo pipefail

# -----------------------------------------------------------------------------
# Constants
# -----------------------------------------------------------------------------

GITHUB_ORG="ai-evo-agents"
GITHUB_API="https://api.github.com"

EVO_HOME="${EVO_HOME:-${HOME}/.evo-agents}"

# All repos to clone for the self-upgrade pipeline
REPOS=(
    evo-common
    evo-king
    evo-gateway
    evo-agents
    evo-kernel-agent-learning
    evo-kernel-agent-building
    evo-kernel-agent-pre-load
    evo-kernel-agent-evaluation
    evo-kernel-agent-skill-manage
    evo-kernel-agent-update
)

# Kernel agents: <repo-name>:<binary-name>
KERNEL_AGENTS=(
    evo-kernel-agent-learning:evo-agent-learning
    evo-kernel-agent-building:evo-agent-building
    evo-kernel-agent-pre-load:evo-agent-pre-load
    evo-kernel-agent-evaluation:evo-agent-evaluation
    evo-kernel-agent-skill-manage:evo-agent-skill-manage
    evo-kernel-agent-update:evo-agent-update
)

# Service binaries (king and gateway)
SERVICE_BINARIES=(
    evo-king
    evo-gateway
)

# -----------------------------------------------------------------------------
# Color output helpers
# -----------------------------------------------------------------------------

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m' # No Color

info()    { printf "${BLUE}[INFO]${NC}    %s\n" "$*"; }
success() { printf "${GREEN}[OK]${NC}      %s\n" "$*"; }
warn()    { printf "${YELLOW}[WARN]${NC}    %s\n" "$*"; }
error()   { printf "${RED}[ERROR]${NC}   %s\n" "$*" >&2; }
header()  { printf "\n${BOLD}${CYAN}==> %s${NC}\n" "$*"; }

# -----------------------------------------------------------------------------
# Cleanup on failure
# -----------------------------------------------------------------------------

CLEANUP_ON_FAILURE=true
cleanup() {
    local exit_code=$?
    if [[ ${exit_code} -ne 0 ]] && [[ "${CLEANUP_ON_FAILURE}" == "true" ]]; then
        error "Installation failed (exit code: ${exit_code})"
        error "Partial installation may remain at ${EVO_HOME}"
        error "To retry, remove it and run this script again:"
        error "  rm -rf ${EVO_HOME} && curl -fsSL https://raw.githubusercontent.com/${GITHUB_ORG}/evo-king/main/install.sh | bash"
    fi
}
trap cleanup EXIT

# -----------------------------------------------------------------------------
# Platform detection
# -----------------------------------------------------------------------------

detect_platform() {
    local os arch target

    os="$(uname -s)"
    arch="$(uname -m)"

    case "${os}" in
        Linux)  os="unknown-linux-gnu" ;;
        Darwin) os="apple-darwin" ;;
        *)
            error "Unsupported operating system: ${os}"
            error "Supported: Linux, macOS"
            exit 1
            ;;
    esac

    case "${arch}" in
        x86_64)          arch="x86_64" ;;
        aarch64|arm64)   arch="aarch64" ;;
        *)
            error "Unsupported architecture: ${arch}"
            error "Supported: x86_64, aarch64 (arm64)"
            exit 1
            ;;
    esac

    target="${arch}-${os}"
    echo "${target}"
}

# -----------------------------------------------------------------------------
# HTTP helpers (curl with optional GitHub token)
# -----------------------------------------------------------------------------

github_curl() {
    local url="$1"
    shift
    local -a headers=()

    if [[ -n "${GITHUB_TOKEN:-}" ]]; then
        headers+=(-H "Authorization: Bearer ${GITHUB_TOKEN}")
    fi
    headers+=(-H "Accept: application/vnd.github+json")

    curl -fsSL "${headers[@]}" "$@" "${url}"
}

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
# GitHub API: get latest release tag for a repo
# -----------------------------------------------------------------------------

get_latest_version() {
    local repo="$1"
    local api_url="${GITHUB_API}/repos/${GITHUB_ORG}/${repo}/releases/latest"
    local version

    version="$(github_curl "${api_url}" | grep '"tag_name"' | head -1 | sed 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/')"

    if [[ -z "${version}" ]]; then
        error "Failed to detect latest version for ${repo}"
        error "Check https://github.com/${GITHUB_ORG}/${repo}/releases"
        exit 1
    fi

    echo "${version}"
}

# -----------------------------------------------------------------------------
# Download and extract a release archive
# -----------------------------------------------------------------------------

download_release() {
    local repo="$1"
    local binary_name="$2"
    local version="$3"
    local target="$4"
    local dest_dir="$5"
    local archive_name="${binary_name}-${version}-${target}.tar.gz"
    local download_url="https://github.com/${GITHUB_ORG}/${repo}/releases/download/${version}/${archive_name}"
    local tmp_archive

    tmp_archive="$(mktemp "${TMPDIR:-/tmp}/evo-download-XXXXXX.tar.gz")"

    info "Downloading ${archive_name}..."
    if ! download_file "${download_url}" "${tmp_archive}"; then
        rm -f "${tmp_archive}"
        error "Failed to download: ${download_url}"
        return 1
    fi

    info "Extracting to ${dest_dir}..."
    tar -xzf "${tmp_archive}" -C "${dest_dir}"
    rm -f "${tmp_archive}"

    return 0
}

# -----------------------------------------------------------------------------
# Directory structure creation
# -----------------------------------------------------------------------------

create_directories() {
    header "Creating directory structure"

    local dirs=(
        "${EVO_HOME}"
        "${EVO_HOME}/bin"
        "${EVO_HOME}/data"
        "${EVO_HOME}/data/backups"
        "${EVO_HOME}/logs"
        "${EVO_HOME}/repos"
    )

    for dir in "${dirs[@]}"; do
        if [[ ! -d "${dir}" ]]; then
            mkdir -p "${dir}"
            info "Created ${dir}"
        else
            info "Exists  ${dir}"
        fi
    done

    success "Directory structure ready"
}

# -----------------------------------------------------------------------------
# Download service binaries (king, gateway)
# -----------------------------------------------------------------------------

install_service_binaries() {
    local target="$1"

    header "Downloading service binaries"

    for service in "${SERVICE_BINARIES[@]}"; do
        local version
        version="$(get_latest_version "${service}")"
        info "${service} latest release: ${version}"

        # Download archive into a temp dir, then move the binary to bin/
        local tmp_extract
        tmp_extract="$(mktemp -d "${TMPDIR:-/tmp}/evo-extract-XXXXXX")"

        if download_release "${service}" "${service}" "${version}" "${target}" "${tmp_extract}"; then
            # Flat archive: binary is at archive root
            if [[ -f "${tmp_extract}/${service}" ]]; then
                mv "${tmp_extract}/${service}" "${EVO_HOME}/bin/${service}"
                chmod +x "${EVO_HOME}/bin/${service}"
                success "Installed ${service} ${version} -> ${EVO_HOME}/bin/${service}"
            else
                error "Binary '${service}' not found in archive"
                rm -rf "${tmp_extract}"
                exit 1
            fi

            # For king: also extract run.sh and update.sh if present
            if [[ "${service}" == "evo-king" ]]; then
                if [[ -f "${tmp_extract}/run.sh" ]]; then
                    cp "${tmp_extract}/run.sh" "${EVO_HOME}/run.sh"
                    chmod +x "${EVO_HOME}/run.sh"
                    info "Copied run.sh -> ${EVO_HOME}/run.sh"
                fi
                if [[ -f "${tmp_extract}/update.sh" ]]; then
                    cp "${tmp_extract}/update.sh" "${EVO_HOME}/data/update.sh"
                    chmod +x "${EVO_HOME}/data/update.sh"
                    info "Copied update.sh -> ${EVO_HOME}/data/update.sh"
                fi
            fi
        else
            rm -rf "${tmp_extract}"
            exit 1
        fi

        rm -rf "${tmp_extract}"

        # Store version for repos.json generation
        eval "VERSION_${service//-/_}=${version}"
    done

    success "Service binaries installed"
}

# -----------------------------------------------------------------------------
# Download kernel agents
# -----------------------------------------------------------------------------

install_kernel_agents() {
    local target="$1"

    header "Downloading kernel agents"

    for entry in "${KERNEL_AGENTS[@]}"; do
        local repo="${entry%%:*}"
        local binary="${entry##*:}"

        local version
        version="$(get_latest_version "${repo}")"
        info "${repo} latest release: ${version}"

        # Agent archives contain a folder structure:
        #   evo-kernel-agent-<name>/binary
        #   evo-kernel-agent-<name>/soul.md
        #   evo-kernel-agent-<name>/skills/
        # Extract directly into EVO_HOME so the folder lands at the right place
        if download_release "${repo}" "${binary}" "${version}" "${target}" "${EVO_HOME}"; then
            # Ensure the binary inside the agent folder is executable
            local agent_dir="${EVO_HOME}/${repo}"
            if [[ -d "${agent_dir}" ]]; then
                # Make any binary files executable
                if [[ -f "${agent_dir}/${binary}" ]]; then
                    chmod +x "${agent_dir}/${binary}"
                fi
                success "Installed ${repo} ${version} -> ${agent_dir}/"
            else
                warn "Expected directory ${agent_dir} not found after extraction"
            fi
        else
            exit 1
        fi

        # Store version for repos.json generation (dynamic variable)
        eval "VERSION_${repo//-/_}=${version}"
    done

    success "Kernel agents installed"
}

# -----------------------------------------------------------------------------
# Clone source repos for self-upgrade pipeline
# -----------------------------------------------------------------------------

clone_repos() {
    header "Cloning source repositories"

    if ! command -v git &>/dev/null; then
        warn "git not found — skipping repo cloning"
        warn "Source repos are needed for the self-upgrade pipeline"
        warn "Install git and re-run, or clone manually into ${EVO_HOME}/repos/"
        return 0
    fi

    for repo in "${REPOS[@]}"; do
        local repo_dir="${EVO_HOME}/repos/${repo}"
        local repo_url="https://github.com/${GITHUB_ORG}/${repo}.git"

        if [[ -d "${repo_dir}/.git" ]]; then
            info "Updating ${repo}..."
            git -C "${repo_dir}" pull --ff-only --quiet 2>/dev/null || warn "Pull failed for ${repo} (non-fatal)"
        else
            info "Cloning ${repo}..."
            if git clone --quiet "${repo_url}" "${repo_dir}" 2>/dev/null; then
                success "Cloned ${repo}"
            else
                warn "Failed to clone ${repo} (non-fatal)"
            fi
        fi
    done

    success "Source repositories ready"
}

# -----------------------------------------------------------------------------
# Generate repos.json manifest
# -----------------------------------------------------------------------------

generate_repos_json() {
    header "Generating repos.json"

    local repos_json="${EVO_HOME}/repos.json"
    local evo_home_escaped
    # Use $EVO_HOME placeholder in the JSON for portability
    evo_home_escaped="\$EVO_HOME"

    # Helper: get stored version for a repo (falls back to "unknown")
    get_version() {
        local var_name="VERSION_${1//-/_}"
        echo "${!var_name:-unknown}"
    }

    # Build the JSON manually to avoid jq dependency
    cat > "${repos_json}" <<JSONEOF
{
  "version": "0.2.0",
  "repos": {
    "evo-common": {
      "github": "${GITHUB_ORG}/evo-common",
      "local_path": "${evo_home_escaped}/repos/evo-common",
      "installed_version": null,
      "binary_path": null,
      "type": "library"
    },
    "evo-king": {
      "github": "${GITHUB_ORG}/evo-king",
      "local_path": "${evo_home_escaped}/repos/evo-king",
      "installed_version": "$(get_version evo-king)",
      "binary_path": "${evo_home_escaped}/bin/evo-king",
      "type": "service"
    },
    "evo-gateway": {
      "github": "${GITHUB_ORG}/evo-gateway",
      "local_path": "${evo_home_escaped}/repos/evo-gateway",
      "installed_version": "$(get_version evo-gateway)",
      "binary_path": "${evo_home_escaped}/bin/evo-gateway",
      "type": "service"
    },
    "evo-agents": {
      "github": "${GITHUB_ORG}/evo-agents",
      "local_path": "${evo_home_escaped}/repos/evo-agents",
      "installed_version": null,
      "binary_path": null,
      "type": "library"
    },
    "evo-kernel-agent-learning": {
      "github": "${GITHUB_ORG}/evo-kernel-agent-learning",
      "local_path": "${evo_home_escaped}/repos/evo-kernel-agent-learning",
      "installed_version": "$(get_version evo-kernel-agent-learning)",
      "binary_path": "${evo_home_escaped}/evo-kernel-agent-learning/evo-agent-learning",
      "type": "kernel-agent"
    },
    "evo-kernel-agent-building": {
      "github": "${GITHUB_ORG}/evo-kernel-agent-building",
      "local_path": "${evo_home_escaped}/repos/evo-kernel-agent-building",
      "installed_version": "$(get_version evo-kernel-agent-building)",
      "binary_path": "${evo_home_escaped}/evo-kernel-agent-building/evo-agent-building",
      "type": "kernel-agent"
    },
    "evo-kernel-agent-pre-load": {
      "github": "${GITHUB_ORG}/evo-kernel-agent-pre-load",
      "local_path": "${evo_home_escaped}/repos/evo-kernel-agent-pre-load",
      "installed_version": "$(get_version evo-kernel-agent-pre-load)",
      "binary_path": "${evo_home_escaped}/evo-kernel-agent-pre-load/evo-agent-pre-load",
      "type": "kernel-agent"
    },
    "evo-kernel-agent-evaluation": {
      "github": "${GITHUB_ORG}/evo-kernel-agent-evaluation",
      "local_path": "${evo_home_escaped}/repos/evo-kernel-agent-evaluation",
      "installed_version": "$(get_version evo-kernel-agent-evaluation)",
      "binary_path": "${evo_home_escaped}/evo-kernel-agent-evaluation/evo-agent-evaluation",
      "type": "kernel-agent"
    },
    "evo-kernel-agent-skill-manage": {
      "github": "${GITHUB_ORG}/evo-kernel-agent-skill-manage",
      "local_path": "${evo_home_escaped}/repos/evo-kernel-agent-skill-manage",
      "installed_version": "$(get_version evo-kernel-agent-skill-manage)",
      "binary_path": "${evo_home_escaped}/evo-kernel-agent-skill-manage/evo-agent-skill-manage",
      "type": "kernel-agent"
    },
    "evo-kernel-agent-update": {
      "github": "${GITHUB_ORG}/evo-kernel-agent-update",
      "local_path": "${evo_home_escaped}/repos/evo-kernel-agent-update",
      "installed_version": "$(get_version evo-kernel-agent-update)",
      "binary_path": "${evo_home_escaped}/evo-kernel-agent-update/evo-agent-update",
      "type": "kernel-agent"
    }
  }
}
JSONEOF

    success "Generated ${repos_json}"
}

# -----------------------------------------------------------------------------
# Print final instructions
# -----------------------------------------------------------------------------

print_success() {
    local divider="================================================================"

    printf "\n${GREEN}${divider}${NC}\n"
    printf "${GREEN}${BOLD}  Evo System installed successfully!${NC}\n"
    printf "${GREEN}${divider}${NC}\n\n"

    printf "${BOLD}Installation directory:${NC}\n"
    printf "  %s\n\n" "${EVO_HOME}"

    printf "${BOLD}Directory layout:${NC}\n"
    printf "  bin/          Service binaries (evo-king, evo-gateway)\n"
    printf "  data/         Database files, configs, backups\n"
    printf "  logs/         Structured log output\n"
    printf "  repos/        Source repos (for self-upgrade pipeline)\n"
    printf "  evo-kernel-*/ Kernel agent directories\n\n"

    printf "${BOLD}Start the system:${NC}\n"
    printf "  %s/run.sh\n\n" "${EVO_HOME}"

    printf "${BOLD}Stop the system:${NC}\n"
    printf "  %s/run.sh --stop\n\n" "${EVO_HOME}"

    printf "${BOLD}Environment variables (optional):${NC}\n"
    printf "  export EVO_HOME=%s\n" "${EVO_HOME}"
    printf "  export OPENAI_API_KEY=<your-key>       # For OpenAI via gateway\n"
    printf "  export ANTHROPIC_API_KEY=<your-key>     # For Anthropic via gateway\n\n"

    printf "${BOLD}Logs:${NC}\n"
    printf "  %s/logs/\n\n" "${EVO_HOME}"

    printf "For more information: https://github.com/${GITHUB_ORG}/evo-king\n\n"
}

# =============================================================================
# Main
# =============================================================================

main() {
    printf "\n${BOLD}${CYAN}Evo System — Bootstrap Installer${NC}\n\n"

    # --- Pre-flight checks ---------------------------------------------------

    # Verify required tools
    for cmd in curl tar; do
        if ! command -v "${cmd}" &>/dev/null; then
            error "Required tool '${cmd}' not found. Please install it and retry."
            exit 1
        fi
    done

    # --- Platform detection ---------------------------------------------------

    header "Detecting platform"
    local target
    target="$(detect_platform)"
    success "Platform: ${target}"

    # --- Check for existing installation -------------------------------------

    if [[ -d "${EVO_HOME}/bin" ]]; then
        warn "Existing installation detected at ${EVO_HOME}"
        warn "Binaries and agents will be overwritten; repos will be updated"
        printf "  Continue? [y/N] "
        # When piped (curl | bash), stdin is the script itself.
        # Default to 'y' in non-interactive mode.
        if [[ -t 0 ]]; then
            read -r confirm
            if [[ "${confirm}" != "y" && "${confirm}" != "Y" ]]; then
                info "Installation cancelled"
                exit 0
            fi
        else
            printf "y (non-interactive)\n"
        fi
    fi

    # --- Install -------------------------------------------------------------

    create_directories
    install_service_binaries "${target}"
    install_kernel_agents "${target}"
    clone_repos
    generate_repos_json
    print_success

    # Disable cleanup message on success
    CLEANUP_ON_FAILURE=false
}

main "$@"
