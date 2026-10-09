#!/bin/bash
# Records the README demo: a real search, then jq over its JSON. Usage: docs/demo.sh /path/to/repo (expects OPENAI_API_KEY).
cd "${1:?repository root}"
export PATH="$(git -C "$(dirname "$0")/.." rev-parse --show-toplevel)/target/release:$PATH"
type_line() { printf '\033[1;32m$\033[0m '; for ((i=0; i<${#1}; i++)); do printf '%s' "${1:$i:1}"; sleep 0.012; done; printf '\n'; }
sleep 0.6
type_line 'blink search "where does the development server decide to restart when a source file changes" src --json > out.json'
start=$(python3 -c 'import time; print(time.time())')
blink search "where does the development server decide to restart when a source file changes" src --json > out.json 2>/dev/null
end=$(python3 -c 'import time; print(time.time())')
sleep 0.3
type_line "jq -r '.results[] | \"\\(.path):\\(.start_line)-\\(.end_line)  probability \\(.probability)\"' out.json"
jq -r '.results[] | "\u001b[1;36m\(.path):\(.start_line)-\(.end_line)\u001b[0m  probability \(.probability)"' out.json
sleep 0.8
type_line "jq -r '.results[0].excerpt' out.json | head -n 14"
jq -r '.results[0].excerpt' out.json | head -n 14
sleep 0.8
type_line "jq -r '\"\\(.budgets.attempts) requests, \\(.budgets.elapsed_ms) ms, \\(.coverage.windows_judged) windows judged\"' out.json"
jq -r '"\u001b[1m\(.budgets.attempts) requests, \(.budgets.elapsed_ms) ms, \(.coverage.windows_judged) windows judged\u001b[0m"' out.json
rm -f out.json
sleep 4
