#!/bin/bash
set -euo pipefail

PROGRAM_NAME="git-interactive"

usage() {
    echo "Usage: $PROGRAM_NAME <command> [args...]"
    echo ""
    echo "Commands are discovered from PATH as ${PROGRAM_NAME}-<command> executables."
    echo ""
    echo "Available commands:"
    # Find all git-interactive-* executables in PATH
    IFS=':' read -ra PATHS <<< "$PATH"
    for dir in "${PATHS[@]}"; do
        if [[ -d "$dir" ]]; then
            for cmd in "$dir/${PROGRAM_NAME}-"*; do
                if [[ -x "$cmd" ]]; then
                    basename "$cmd" | sed "s/^${PROGRAM_NAME}-/  /"
                fi
            done
        fi
    done | sort -u
}

if [[ $# -lt 1 ]]; then
    usage
    exit 1
fi

COMMAND="$1"
shift

SUBCOMMAND="${PROGRAM_NAME}-${COMMAND}"

if ! command -v "$SUBCOMMAND" &> /dev/null; then
    echo "Error: Unknown command '$COMMAND'" >&2
    echo "       '$SUBCOMMAND' not found in PATH" >&2
    exit 1
fi

exec "$SUBCOMMAND" "$@"
