#!/bin/bash
# Records docs/blink.gif: one real search in a plain shell. The command is echoed, then the
# recording's keystroke events are re-spaced to a typing cadence before rendering; the output,
# timing of the search, and footer are the real ones.
# Usage: docs/demo.sh /path/to/repository "question" ROOT   (needs OPENAI_API_KEY, asciinema, agg)
set -euo pipefail
repo=${1:?repository}; question=${2:?question}; root=${3:?root}
here=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
cat > "$work/session.sh" <<SESSION
export PS1='\[\e[2m\]$(basename "$repo")\[\e[0m\] $ '
export PATH="$here/target/release:\$PATH"
cd "$repo"
clear
printf '%s' "\$PS1" | sed 's/\\\\\[//g; s/\\\\\]//g' | sed 's/\\\\e/\x1b/g'
cmd='blink search "$question" $root --limit 1 | head -n 28'
printf '%s' "\$cmd"
sleep 0.6
printf '\r\n'
eval "\$cmd"
printf '%s' "\$PS1" | sed 's/\\\\\[//g; s/\\\\\]//g' | sed 's/\\\\e/\x1b/g'
sleep 1
SESSION
asciinema rec --command "bash --noprofile --norc $work/session.sh" --window-size 100x33 --overwrite "$work/demo.cast" >/dev/null
python3 - "$work/demo.cast" "$work/typed.cast" "blink search \"$question\" $root --limit 1 | head -n 28" <<'PY'
import json, random, sys
src, dst, command = sys.argv[1], sys.argv[2], sys.argv[3]
random.seed(7)
lines = open(src).read().splitlines()
header, events = json.loads(lines[0]), [json.loads(l) for l in lines[1:]]
out = []
for dt, kind, data in events:
    if kind == 'o' and data == command:
        for i, ch in enumerate(data):
            delay = random.uniform(0.045, 0.12) + (random.uniform(0.0, 0.15) if ch == ' ' else 0.0)
            out.append([round(1.2 if i == 0 else delay, 3), 'o', ch])
        continue
    if kind == 'x':
        break
    out.append([round(dt, 3), kind, data])
with open(dst, 'w') as f:
    f.write(json.dumps(header) + '\n')
    for e in out:
        f.write(json.dumps(e) + '\n')
PY
agg --font-size 15 --theme asciinema --idle-time-limit 3 --last-frame-duration 5 "$work/typed.cast" "$here/docs/blink.gif" >/dev/null
rm -rf "$work"
echo "wrote docs/blink.gif"
