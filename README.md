# Blink

Ask a codebase a question in plain English. Get back the exact source that answers it, in about two seconds.

![A real Blink search: the question, the ranked excerpts with paths and line numbers, and the request count and time](docs/blink.gif)

Blink is a command-line code search for coding agents. It reads the working tree, splits files into windows, and asks the OpenAI Decisions API which windows implement the behavior in the question. It returns the matching source verbatim, with the path, line numbers, byte offsets and the file's SHA-256, so an agent can cite and verify what it read. There is no index to build and no background process.

## Why it exists

Agents spend most of a task finding the right code. Exact-text search needs the right identifier; reading files by hand burns context. The tools that answer behavioral questions well tend to take 15 to 20 seconds and 50 or more model calls per question, which is too slow inside an agent loop. Blink answers the same kind of question in about 2 seconds with 15 to 20 calls, stays quiet when the behavior does not exist, and never returns source it did not read byte for byte.

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

1. Lists eligible files under the root: UTF-8 text, `.gitignore` honoured, hidden, dependency, build and credential paths excluded.
2. On larger scopes, scores short previews of the repository's regions, then judges up to eight 80-line windows per request, each with its own yes-or-no question plus one listwise question ("which of these, if any, is the answer?").
3. Follows the evidence: windows that share identifiers with accepted code, the definitions that accepted code calls, a second look at near misses with all accepted excerpts attached, and the unread windows nearest to accepted code in the same file.
4. Rereads every accepted file and compares its hash before output, merges overlapping windows, and returns the best record per directory first.

By default a search may spend 40 requests, 1.25 MiB of request bodies and 30 seconds, 16 requests at a time; most searches finish well inside that because the passes stop when there is nothing left worth reading. `--thorough` allows 64 requests, 2 MiB and 60 seconds. The exact passes, budgets and the JSON schema are in the [reference](docs/reference.md).

## Measured against the Reference CLI

Both tools answer the same questions on the same checkouts with the same scorer. A question counts only when every required span is inside returned source. Full receipts, hashes and the consumed-versus-fresh status of every set are in [benchmarks](benchmarks/README.md).

| Set | Blink complete | Reference CLI complete | Source returned on no-answer questions | Median requests | Median time |
| --- | ---: | ---: | ---: | ---: | ---: |
| 360 generated questions, werkzeug and ripgrep | 300 (83%) | 304 (84%) | 5/120 vs 16/120 | 18 vs 58 to 62 | 2.1 s vs 19 to 21 s |
| 12 hand-written multi-span questions, starlette and bat | 9 | 11 | 1/12 vs 6/12 | 14 vs 48 | 1.6 s vs 15 s |
| 12 hand-written multi-span questions in large files, jinja and just | 2 | 9 | 1/12 vs 4/12 | 11 vs 54 | 1.9 s vs 18 s |

Blink is level with the Reference CLI on generated questions and far cheaper everywhere. On hand-written questions about large files the Reference CLI still finds more, because it reads about five times as many windows. That gap is the current work, and Blink has not been released to the team until it closes. Error trials were zero for Blink on every set above.

## Limits

| | Default | `--thorough` |
| --- | --- | --- |
| Deadline | 30 s | 60 s |
| HTTP attempts, including retries | 42 | 64 |
| Request bodies | 1,408 KiB | 2 MiB |
| Concurrent requests | 16 | 16 |
| Records returned (`--limit`, up to 100) | 8 | 8 |
| Output | 32 KiB | 32 KiB |

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

[verification.md](docs/verification.md) covers evaluation, benchmarks, proofs, fuzzing and native builds. [reference.md](docs/reference.md) covers search internals, the inventory policy, coverage semantics and the library API. `docs/demo.sh` records the GIF above against any repository.
