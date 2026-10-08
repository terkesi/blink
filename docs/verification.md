# Verify Blink

Run the [project verification skill](../.agents/skills/verify/SKILL.md) after changing source policy, search, or the CLI. It preserves command output and cleanup evidence.

## Local checks

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
python3 -m unittest discover -s tests -p 'test_*.py'
python3 -m unittest discover -s tests/eval
scripts/check-eval-corpus
scripts/verify --output /tmp/blink-verification
```

The HTTP tests run a loopback server through the production provider and search coordinator. They exercise payloads, responses, shared preview/source byte reservations, retries, deadlines, cancellation, and source changes. Routing tests verify that delayed preview scores change later source admission, failed previews preserve fallback, and preview scores cannot produce source results. They make no model calls.

## Retrieval evaluation

The [base corpus](../tests/eval/README.md) checks source relevance and exact answer spans. The generated competition corpus has 333 eligible files per repository and paired neutral and hostile source text. It tests selection when the candidate pool exceeds both request budgets.

```sh
python3 tests/eval/generate_competition.py --output /tmp/blink-eval
cargo build --locked --release --examples
scripts/check-eval-corpus --competition --corpus /tmp/blink-eval/corpus.json --prepare target/release/examples/prepare
```

Set `OPENAI_API_KEY` and commit the evaluated revision before running a live split. `scripts/evaluate` invokes the production search pipeline and records exact byte ranges, all returned judgments, actual candidate counts, and output omissions. It stops on an authentication or model-access error. It sends only the selected corpus's source roots.

```sh
scripts/evaluate --corpus /tmp/blink-eval/corpus.json --split calibration --output /tmp/blink-calibration
scripts/score-eval --corpus /tmp/blink-eval/corpus.json --run /tmp/blink-calibration/run.json --grid 0.4,0.5,0.6,0.7,0.8,0.9
scripts/score-eval --corpus /tmp/blink-eval/corpus.json --run /tmp/blink-calibration/run.json --freeze-threshold 0.5 > /tmp/blink-threshold.json
scripts/evaluate --corpus /tmp/blink-eval/corpus.json --split heldout --frozen /tmp/blink-threshold.json --output /tmp/blink-heldout
```

Choose the threshold from calibration evidence before examining held-out results. The example uses 0.5 only to show the command syntax. Use a separate frozen calibration artifact for `--mode thorough`. An offline oracle exercises the scorer and can never pass a live quality gate. See the [receipt contract](../tests/eval/scorer.md) for metric definitions, provenance, and limits.

Calibration replay requires the recorded result-selection policy and must reproduce the original ordered source ranges before evaluating another threshold. A ranking change that breaks that agreement fails the replay. Preserve historical receipts with their original policy; do not assign missing metadata after seeing a result.

Report all-required source completion alongside hit@8. Count source returned for absent behavior separately from execution errors and incomplete coverage. A returned counterexample can help an agent answer a question while still counting against an abstention metric. Neither metric proves downstream coding-task success. Repeated timing trials add timing samples, not independent quality questions.

Release targets are at least 85% hit@8 in default mode, 95% in thorough mode, and at most 5% negative false positives. Multifile completion, attack exposure, and irrelevant returned records require separate assessment. A successful small live example does not establish those targets.

## Generated question sets

`scripts/generate-questions` builds a corpus in the frozen comparison driver's schema from one or more pinned repository checkouts. It inventories eligible files through `blink files ROOT --json`, derives queries from documentation comments (Python docstrings, Rust `///`, JS/TS `/** */`) with the declared symbol name replaced by "this", and labels gold spans as the declaration line plus its first eight body lines. A configurable share of questions adds the first five lines of one uniquely declared callee as a second gold span (`multi_hop`). Negative questions reuse queries generated from a different root and are valid only when the replaced symbol does not appear as an identifier in the target repository. Everything is deterministic from `--seed`; choose a new seed for a fresh set. `scripts/paired-summary` reads the driver's per-trial `trial.json` files, reports per-tool span recall, negative and error counts, and request/byte/time medians, then pairs two tools per question and runs an exact two-sided sign test on strict hits and span recall.

```sh
scripts/generate-questions --root /path/repo-a --id repo-a --root /path/repo-b --id repo-b \
    --seed 1 --count 60 --negatives 20 --output /path/corpus/corpus.json --blink target/release/blink
scripts/paired-summary /path/compare-output-a /path/compare-output-b --tools blink,jg
```

Repository roots must sit inside the corpus file's directory so relative roots resolve. These queries are doc-derived and easier than hand-written behavioral questions; many share the files their docs describe. Use generated sets for relative comparison between tools and for regression, not for absolute quality claims or release gating. The frozen driver's `--split` flag accepts only `calibration` and `heldout`, so a `generated` split loads through `load_corpus` rather than the driver's CLI.

## Local preparation benchmark

```sh
scripts/benchmark --output /tmp/blink-benchmark
```

The helper builds a release example that calls production source discovery and candidate preparation. Each sample processes 10,000 files totaling 50 MiB and verifies file, byte, window, and selected-candidate counts. It makes no HTTP requests. It runs five trials in the order new fixture, unchanged repeat, then changed file.

`summary.json` records every duration, CPU time, process peak resident memory, load average, code revision, and fixture cleanup. OS file caches are uncontrolled. The new-fixture condition starts a fresh process; it does not establish cold-disk performance. Source creation and compilation are outside the timed region. Targets are less than one second for preparation and less than 256 MiB peak resident memory on the measured machine. Report machine details and the full range with any timing claim.

## Arithmetic proofs and fuzzing

```sh
cargo install --locked kani-verifier --version 0.68.0
cargo kani setup
scripts/prove
```

The two Kani proofs import the production request-reservation and range-validation helpers. They check overflow, exact reservation deltas, policy ceilings, and byte-range containment for arbitrary machine integers. They do not prove filesystem confinement, UTF-8 segmentation, cancellation, provider behavior, or relevance.

```sh
cargo install --locked cargo-fuzz --version 0.13.2
rustup toolchain install nightly-2026-08-21 --profile minimal --component rust-src
scripts/fuzz
```

The driver runs 20,000 cases per target with a fixed random seed. Preparation starts with ASCII, Unicode, CRLF, long-line, and binary seeds. The preparation target writes only its disposable fixture and invokes production discovery and window planning. The bounds target imports the same helpers as the proofs. Preserve a failing input before changing code. A bounded fuzz run covers the inputs it exercised, not every possible input.

## Native builds

The workflow tests and executes native binaries on Linux and macOS, each on x86-64 and ARM64. It retains archives, checksums, and CLI evidence as CI artifacts. It does not publish releases. A local pass proves only the local platform; inspect all four jobs for the intended revision before claiming platform coverage.
