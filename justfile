_default:
    @just --list

# Mirror Claude Code project memories to .claude-memories/ (for git backup / portability)
mirror-memory:
    #!/usr/bin/env bash
    set -euo pipefail
    memdir="$HOME/.claude/projects/$(pwd | tr / -)/memory"
    if [[ ! -d "$memdir" ]]; then
        echo "no memory directory at $memdir" >&2
        exit 1
    fi
    mkdir -p .claude-memories
    rsync -a --delete \
        --exclude 'last-session-id' \
        "$memdir"/ .claude-memories/
    echo "mirrored $memdir → .claude-memories/"

# Resume the most recent Claude Code session for this project (uses ID recorded on session end)
claude-resume:
    #!/usr/bin/env bash
    set -euo pipefail
    sid_file=".claude-memories/last-session-id"
    if [[ ! -s "$sid_file" ]]; then
        echo "no saved session id at $sid_file" >&2
        echo "run 'claude' to start a fresh session (the SessionEnd hook will record the id)" >&2
        exit 1
    fi
    sid=$(< "$sid_file")
    echo "resuming session $sid"
    exec claude --resume "$sid"
