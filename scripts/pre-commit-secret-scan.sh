#!/usr/bin/env bash
# scripts/pre-commit-secret-scan.sh
#
# Pre-commit hook: scans staged changes for secrets using gitleaks.
#
# This script is installed into .git/hooks/pre-commit by scripts/install-hooks.sh.
# It is intentionally non-blocking when gitleaks is not installed, so developers
# without gitleaks can still commit — CI will catch secrets regardless.
#
# Usage (direct): ./scripts/pre-commit-secret-scan.sh
# Usage (via hook): automatically invoked by `git commit`

set -euo pipefail

# ─── Colour helpers ───────────────────────────────────────────────────────────
RED='\033[0;31m'
YELLOW='\033[1;33m'
GREEN='\033[0;32m'
CYAN='\033[0;36m'
BOLD='\033[1m'
RESET='\033[0m'

# ─── Locate project root (works when called from any subdirectory) ─────────────
REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || echo ".")"
GITLEAKS_CONFIG="${REPO_ROOT}/.gitleaks.toml"

# ─── Check gitleaks is available ──────────────────────────────────────────────
if ! command -v gitleaks &>/dev/null; then
  echo -e "${YELLOW}⚠️  WARNING: gitleaks is not installed — skipping local secret scan.${RESET}"
  echo -e "${YELLOW}   Secrets will still be caught by CI before your branch can be merged.${RESET}"
  echo ""
  echo -e "   To install gitleaks and enable local scanning:"
  echo -e "     ${CYAN}# macOS${RESET}"
  echo -e "     ${CYAN}brew install gitleaks${RESET}"
  echo ""
  echo -e "     ${CYAN}# Linux (x86_64)${RESET}"
  echo -e "     ${CYAN}GITLEAKS_VERSION=8.21.2${RESET}"
  echo -e "     ${CYAN}curl -sSfL \"https://github.com/gitleaks/gitleaks/releases/download/v\${GITLEAKS_VERSION}/gitleaks_\${GITLEAKS_VERSION}_linux_x64.tar.gz\" | tar xz -C /usr/local/bin gitleaks${RESET}"
  echo ""
  echo -e "     ${CYAN}# Windows (winget)${RESET}"
  echo -e "     ${CYAN}winget install gitleaks${RESET}"
  echo ""
  # Exit 0 — non-blocking; developers without gitleaks should not be locked out.
  exit 0
fi

# ─── Verify the gitleaks config exists ────────────────────────────────────────
if [[ ! -f "${GITLEAKS_CONFIG}" ]]; then
  echo -e "${YELLOW}⚠️  WARNING: .gitleaks.toml not found at ${GITLEAKS_CONFIG}${RESET}"
  echo -e "${YELLOW}   Falling back to gitleaks built-in ruleset.${RESET}"
  GITLEAKS_CONFIG_ARG=""
else
  GITLEAKS_CONFIG_ARG="--config ${GITLEAKS_CONFIG}"
fi

# ─── Run the scan ─────────────────────────────────────────────────────────────
echo -e "${CYAN}🔍 Scanning staged changes for secrets...${RESET}"

# `gitleaks protect --staged` inspects the git index (staged diff) only.
# --redact replaces detected secret values with REDACTED in output.
# shellcheck disable=SC2086
if gitleaks protect --staged ${GITLEAKS_CONFIG_ARG} --redact 2>&1; then
  echo -e "${GREEN}✅ No secrets detected in staged changes.${RESET}"
  exit 0
else
  SCAN_EXIT=$?
  echo ""
  echo -e "${RED}${BOLD}╔══════════════════════════════════════════════════════════════╗${RESET}"
  echo -e "${RED}${BOLD}║            🚨 SECRET DETECTED — COMMIT BLOCKED 🚨             ║${RESET}"
  echo -e "${RED}${BOLD}╚══════════════════════════════════════════════════════════════╝${RESET}"
  echo ""
  echo -e "${RED}gitleaks found one or more potential secrets in your staged changes.${RESET}"
  echo -e "${RED}The commit has been blocked to protect your credentials.${RESET}"
  echo ""
  echo -e "${BOLD}What to do next:${RESET}"
  echo ""
  echo -e "  1. ${BOLD}Remove the secret from your staged files.${RESET}"
  echo -e "     Never commit real API keys, tokens, passwords, or private keys."
  echo ""
  echo -e "  2. ${BOLD}If the value is a legitimate test/placeholder, add it to the allowlist:${RESET}"
  echo -e "     Edit ${CYAN}.gitleaks.toml${RESET} and add the value or file path under ${CYAN}[allowlist]${RESET}."
  echo -e "     See ${CYAN}docs/secret-scanning.md${RESET} for full instructions."
  echo ""
  echo -e "  3. ${BOLD}Use environment variables or a secrets manager instead:${RESET}"
  echo -e "     Copy ${CYAN}.env.example${RESET} to ${CYAN}.env${RESET} and fill in real values there."
  echo -e "     ${CYAN}.env${RESET} is git-ignored and will never be committed."
  echo ""
  echo -e "  4. ${BOLD}If a real secret was already committed in a previous commit:${RESET}"
  echo -e "     Rotate the secret immediately, then use ${CYAN}git filter-repo${RESET} or"
  echo -e "     contact your security team to scrub the history."
  echo ""
  echo -e "${YELLOW}To bypass this check in an emergency (not recommended):${RESET}"
  echo -e "  ${CYAN}git commit --no-verify${RESET}  (CI will still block the PR)"
  echo ""
  exit ${SCAN_EXIT}
fi
