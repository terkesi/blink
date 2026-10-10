# Blink

Ask a codebase a question in plain English. Get back the exact source that answers it, in a few seconds.

![A terminal session: a plain-English question about werkzeug's reloader, three seconds, and the exact function that answers it with its path, line range and probability](docs/blink.gif)

Blink is a command-line code search for coding agents. It reads the working tree, splits files into windows, and asks the OpenAI Decisions API which windows implement the behavior in the question. It returns the matching source verbatim, with the path, line numbers, byte offsets and the file's SHA-256, so an agent can cite and verify what it read. There is no index to build and no background process.

## Why it exists

Agents spend most of a task finding the right code. Exact-text search needs the right identifier; reading files by hand burns context. The tools that answer behavioral questions well take 15 to 40 seconds per question, too slow inside an agent loop. Blink answers the same kind of question in three to five seconds, stays quiet when the behavior does not exist, and never returns source it did not read byte for byte. It spends more model calls than those tools to do it; that cost is in the table below.

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
3. Follows the evidence: windows that share identifiers with accepted code, the definitions that accepted code calls, a second look at near misses (each shown with its enclosing declaration, a verified caller or use site, and the accepted excerpts), and the unread windows of implicated files, nearest to their anchor first.
4. Rereads every accepted file and compares its hash before output, merges overlapping windows, and returns the best record per directory first.

By default the first pass is 64 requests and 2 MiB and reads scopes of up to 512 windows whole; fresh work overall is capped at 142 requests and 4,512 KiB (150 and 5,024 KiB with retries) within 30 seconds, 32 requests at a time, and the passes stop when there is nothing left worth reading. `--thorough` doubles every allowance (a 128-request first pass, scopes of up to 1,024 windows read whole, 286 requests and 9.4 MiB) within 60 seconds. The exact passes, budgets and the JSON schema are in the [reference](docs/reference.md).

## Blink against the Reference CLI

Same questions, same checkouts, same scorer, both tools run back to back. A question counts as answered only when every required span is inside the returned source. Blink numbers are from the current build (`70ebc5e`) and the frozen `e495183` gate run; receipts, hashes and the status of every set are in [benchmarks](benchmarks/README.md).

| | Blink | Reference CLI | Ahead |
| --- | ---: | ---: | --- |
| **Finds every required span** | | | |
| 360 generated questions, three fresh sets (werkzeug, ripgrep) | **311** | 298 | Blink |
| 720 generated positives, regression seeds (werkzeug, ripgrep) | **675** | not run | |
| 96 hand-written multi-span questions (eight sealed sets, Python, Rust and TypeScript) | **77** | 72 | Blink |
| The newest set alone (requests, serde), run fresh against the frozen `e495183` build | 9 | 9 | level |
| Required spans found, hand-written (353) | **325** | 315 | Blink |
| **Stays quiet when there is no answer** | | | |
| Source returned on 96 no-answer questions, hand-written | **22** | 68 | Blink |
| Source returned on 240 no-answer questions, regression seeds | **22** | not run | |
| Source returned on 120 no-answer questions, generated | **9** | 16 | Blink |
| **Cost of one search (medians)** | | | |
| Wall time | **2.8 to 4.6 s** | 11 to 44 s | Blink, 4 to 12x |
| Slowest tenth of searches | **4 to 7 s** | 29 to 68 s | Blink |
| Model requests | 125 to 142 | **32 to 178** | Reference CLI on most sets |
| Data sent to the model | 3.0 to 3.3 MB | **1.1 to 4.9 MB** | Reference CLI on most sets |
| Failed searches | **0 of 864** (two retries per batch) | 4 of 252 | Blink |

Blink finds more than the Reference CLI on both question families, returns source on no-answer questions a third as often, has not failed a search in 864 runs, and finishes in a fifth of the time. It spends about four times the model requests on most sets. The pooled hand-written lead (77 against 72) includes sets that steered later changes; the only clean evidence is the newest set, run with the build frozen before the questions were written, where the two tools completed the same nine of twelve and Blink found more spans (36 against 34) with fewer false sources (2 against 5). Every span Blink missed there had been read and scored below the threshold. With an agent in the loop (Claude, one mandated search then free verification, 24 questions) the tools were level at 6 of 12, Blink's agent finishing in 25 s against 41 s. The release [gate](benchmarks/README.md#the-gate) asks for a paired win on a fresh set with no more no-answer sources, zero failures and half the cost; the newest set met the first three clauses and failed the cost clause, so Blink is released with that cost stated.

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
