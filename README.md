# Blink

Ask a codebase a question in plain English. Get back the exact source that answers it, in about two seconds.

![A terminal session: a plain-English question about werkzeug's reloader, three seconds, and the exact function that answers it with its path, line range and probability](docs/blink.gif)

Blink is a command-line code search for coding agents. It reads the working tree, splits files into windows, and asks the OpenAI Decisions API which windows implement the behavior in the question. It returns the matching source verbatim, with the path, line numbers, byte offsets and the file's SHA-256, so an agent can cite and verify what it read. There is no index to build and no background process.

## Why it exists

Agents spend most of a task finding the right code. Exact-text search needs the right identifier; reading files by hand burns context. The tools that answer behavioral questions well tend to take 15 to 20 seconds and 50 or more model calls per question, which is too slow inside an agent loop. Blink answers the same kind of question in about 2 seconds with 20 to 40 calls, stays quiet when the behavior does not exist, and never returns source it did not read byte for byte.

## Quick start

```sh
cargo install --git https://github.com/terkesi/blink --locked --bin blink
export OPENAI_API_KEY=...   # needs access to gpt-6-luna on /v1/decisions

blink files src --json                      # what a search would read
blink search "where does a timed-out job become eligible for retry" src
blink search "..." src --json               # for agents: results, coverage, budgets, errors
```

Rust 1.93 (pinned by the repository) on macOS or Linux. `blink doctor --json` reports whether the key is set; it never prints it. For agents, install the usage skill, which tells them how to scope a search and follow the evidence:

```sh
npx skills add terkesi/blink --skill blink
```

## What a search does

![One real search, one frame per request: files are columns, 80-line windows are blocks; windows light up as they are judged and settle into accepted, nominated or rejected](docs/algorithm.gif)

1. Lists eligible files under the root: UTF-8 text, `.gitignore` honoured, hidden, dependency, build and credential paths excluded.
2. On scopes beyond 512 windows, scores short previews of the repository's regions, then judges up to eight 80-line windows per request, each with its own yes-or-no question plus one listwise question ("which of these, if any, is the answer?").
3. Follows the evidence: windows that share identifiers with accepted code, the definitions that accepted code calls, a second look at near misses with all accepted excerpts attached, and the unread windows of implicated files (accepted ones, then those the model's scores, the call graph or the query's own words point at), nearest to their anchor first.
4. Rereads every accepted file and compares its hash before output, merges overlapping windows, and returns the best record per directory first.

By default the first pass is 64 requests and 2 MiB and reads scopes of up to 512 windows whole; fresh work overall is capped at 142 requests and 4,512 KiB (150 and 5,024 KiB with retries) within 30 seconds, 32 requests at a time, and the passes stop when there is nothing left worth reading. `--thorough` doubles every allowance (a 128-request first pass, scopes of up to 1,024 windows read whole, 286 requests and 9.4 MiB) within 60 seconds. The exact passes, budgets and the JSON schema are in the [reference](docs/reference.md).

## Blink against the Reference CLI

Same questions, same checkouts, same scorer, both tools run back to back. A question counts as answered only when every required span is inside the returned source. Blink numbers are from the current build (`e495183`); receipts, hashes and the status of every set are in [benchmarks](benchmarks/README.md).

| | Blink | Reference CLI | Ahead |
| --- | ---: | ---: | --- |
| **Finds every required span** | | | |
| 360 generated questions, three fresh sets (werkzeug, ripgrep) | **311** | 298 | Blink |
| 720 generated positives, regression seeds (werkzeug, ripgrep) | **666** | not run | |
| 84 hand-written multi-span questions (seven sealed sets, Python, Rust and TypeScript) | **64** | 63 | level |
| The newest set alone (pydantic, clap), fresh for this build | 7 | **9** | Reference CLI, 3 paired wins to 5 losses |
| Required spans found, hand-written (313) | 281 | **281** | level |
| **Stays quiet when there is no answer** | | | |
| Source returned on 84 no-answer questions, hand-written | **20** | 63 | Blink |
| Source returned on 240 no-answer questions, regression seeds | **22** | not run | |
| Source returned on 120 no-answer questions, generated | **9** | 16 | Blink |
| **Cost of one search (medians)** | | | |
| Wall time | **3.5 to 4.6 s** | 15 to 44 s | Blink, 4 to 12x |
| Slowest tenth of searches | **5 to 7 s** | 29 to 68 s | Blink |
| Model requests | 132 to 142 | **51 to 178** | mixed |
| Data sent to the model | 3.0 to 3.3 MB | **1.5 to 4.9 MB** | level |
| Failed searches | **0 of 840** (two retries per batch) | 4 of 228 | Blink |

Blink finds more on generated questions (311 against 298, 36 paired wins to 23 losses) and is level on hand-written multi-span questions over seven sealed sets (64 against 63). The newest set, run fresh against this build, went to the Reference CLI 9 to 7: of the twelve spans Blink missed there, eight were in files it had opened but not read at the right place and four it read and scored below the threshold. The latest change, keeping a window's best judgment across passes instead of its last, recovered 30 of 720 generated questions and 3 of the newest set's 12 at zero extra requests. Blink returns source on no-answer questions a third as often and finishes in a fifth of the time, while spending about twice the requests. With an agent in the loop (Claude, one mandated search then free verification, 24 questions) the two tools were level at 6 of 12, with Blink's agent finishing in 25 s against 41 s. The [gate](benchmarks/README.md#the-gate) for releasing it to the team is a paired win on a fresh sealed set with no more no-answer sources, zero failures and half the cost; the newest set failed the first clause (3 paired wins to 5 losses) and met the other three.

## Limits

| | Default | `--thorough` |
| --- | --- | --- |
| Deadline | 30 s | 60 s |
| HTTP attempts, including retries | 150 | 286 |
| Request bodies | 5,024 KiB | 9,632 KiB |
| Concurrent requests | 32 | 32 |
| Records returned (`--limit`, up to 100) | 16 | 16 |
| Output | 64 KiB | 64 KiB |

Exit codes: 0 results or completed; 1 no matches after judging the whole scope; 2 invalid invocation or configuration; 3 failed or incomplete; 130 interrupted.

## What leaves your machine

The question, relative paths, region previews and the selected source windows go to OpenAI. Nothing else does. Choose the smallest useful root, and run `blink files ROOT` first to see exactly which files are eligible. The exclusion rules catch common secret files by name; they cannot detect a secret embedded in ordinary source.

## Develop

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
scripts/verify --output /tmp/blink-proof
```

[verification.md](docs/verification.md) covers evaluation, benchmarks, proofs, fuzzing and native builds. [reference.md](docs/reference.md) covers search internals, the inventory policy, coverage semantics and the library API. `docs/demo.sh` records the terminal GIF (a real search; only the typing cadence is re-spaced), and `docs/algorithm-animation.py` draws the search animation from an `evaluate_case` receipt.
