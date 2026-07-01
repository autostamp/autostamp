#!/usr/bin/env bash
#
# open-package-settings.sh
#
# Open the settings page of every NON-PUBLIC (private/internal) published
# package in a GitHub org, in batches. After each batch the script pauses and
# asks at the CLI before opening the next batch.
#
# Each package's settings page (where the "Change visibility" control lives):
#   https://github.com/orgs/<ORG>/packages/<TYPE>/<NAME>/settings
#
# Requirements:
#   - gh (GitHub CLI), authenticated: `gh auth login`
#   - A URL opener: `open` (macOS) or `xdg-open` (Linux), or set BROWSER.
#
# Usage:
#   ./open-package-settings.sh [ORG]        # ORG defaults to "autostamp"
#
# Environment variables:
#   PACKAGE_TYPE=container   Package type to walk (default: container).
#   BATCH=20                 How many to open before pausing (default: 20).
#   DRY_RUN=true             Print URLs instead of opening a browser.
#   BROWSER="open -a Safari" Override the command used to open URLs.

set -euo pipefail

ORG="${1:-autostamp}"
PACKAGE_TYPE="${PACKAGE_TYPE:-container}"
BATCH="${BATCH:-20}"
DRY_RUN="${DRY_RUN:-false}"

# --- preflight -------------------------------------------------------------

if ! command -v gh >/dev/null 2>&1; then
  echo "error: 'gh' (GitHub CLI) is not installed. See https://cli.github.com/" >&2
  exit 1
fi

if ! gh auth status >/dev/null 2>&1; then
  echo "error: not authenticated. Run 'gh auth login' first." >&2
  exit 1
fi

# Pick a URL opener (unless a browser command was supplied or we're dry-running).
OPENER=""
if [ "$DRY_RUN" != "true" ]; then
  if [ -n "${BROWSER:-}" ]; then
    OPENER="$BROWSER"
  elif command -v open >/dev/null 2>&1; then
    OPENER="open"          # macOS
  elif command -v xdg-open >/dev/null 2>&1; then
    OPENER="xdg-open"      # Linux
  else
    echo "error: no URL opener found. Set BROWSER, or use DRY_RUN=true." >&2
    exit 1
  fi
fi

# --- gather non-public package names --------------------------------------

echo "Listing non-public $PACKAGE_TYPE packages in '$ORG'..."
list="$(gh api --paginate \
  "/orgs/$ORG/packages?package_type=$PACKAGE_TYPE&per_page=100" \
  --jq '.[] | select(.visibility != "public") | .name' 2>/dev/null | sort || true)"

if [ -z "$list" ]; then
  echo "No non-public packages found (or unable to list them)." >&2
  echo "If you expected results, your token may lack package read access." >&2
  echo "  Try:  unset GH_TOKEN   (to fall back to a keyring token), then re-run." >&2
  exit 0
fi

# Read lines into an array (bash 3.2 compatible: no mapfile).
names=()
while IFS= read -r n; do
  [ -n "$n" ] && names+=("$n")
done <<EOF
$list
EOF

total=${#names[@]}
echo "Found $total non-public package(s). Opening $BATCH at a time."
[ "$DRY_RUN" = "true" ] && echo "(DRY RUN: printing URLs, not opening a browser)"
echo

# Read a confirmation from the terminal, even if stdin is piped.
confirm() {
  local reply=""
  printf '%s' "$1"
  if [ -r /dev/tty ]; then
    read -r reply < /dev/tty 2>/dev/null || reply=""
  else
    read -r reply || reply=""
  fi
  case "$reply" in
    y | Y | yes | Yes | YES) return 0 ;;
    *) return 1 ;;
  esac
}

# --- walk in batches -------------------------------------------------------

i=0
while [ "$i" -lt "$total" ]; do
  end=$(( i + BATCH ))
  [ "$end" -gt "$total" ] && end="$total"

  j="$i"
  while [ "$j" -lt "$end" ]; do
    name="${names[$j]}"
    url="https://github.com/orgs/$ORG/packages/$PACKAGE_TYPE/$name/settings"
    printf '  [%d/%d] %s\n' "$((j + 1))" "$total" "$name"
    if [ "$DRY_RUN" = "true" ]; then
      printf '        %s\n' "$url"
    else
      # shellcheck disable=SC2086
      $OPENER "$url" >/dev/null 2>&1 || echo "        (failed to open $url)" >&2
      sleep 0.2
    fi
    j=$(( j + 1 ))
  done

  i="$end"

  if [ "$i" -lt "$total" ]; then
    echo
    if ! confirm "Opened $i of $total. Open next $BATCH? [y/N] "; then
      echo "Stopping at $i/$total. Re-run to continue with the rest."
      exit 0
    fi
    echo
  fi
done

echo
echo "Done. Opened all $total settings page(s)."
