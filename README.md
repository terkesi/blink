# Blink

Blink finds source code from natural-language questions. It reads eligible files, asks the OpenAI Decisions API which excerpts are relevant, and returns exact source with paths, line numbers, and hashes.

Blink has no index or background process. Each search reads the current working tree. Results describe individual file versions, not an atomic repository snapshot.

## Run the CLI

Install Rust 1.93.0 through rustup. The repository pins that toolchain and commits `Cargo.lock`. The supported development platforms are macOS and Linux.

```sh
cargo build --locked --release
./target/release/blink files src --json
./target/release/blink doctor --json
./target/release/blink search "where are failed requests retried" src --json
```

`files` defaults to the current directory. Terminal output contains one JSON-quoted relative path per line. Quoting makes embedded control characters visible. Coverage summaries go to stderr. `--json` works before or after each subcommand and writes one JSON object to stdout. Diagnostics stay on stderr.

Set `OPENAI_API_KEY` in the environment before searching. Blink uses `https://api.openai.com/v1/decisions` with `gpt-6-luna`. Your project needs access to that model. `doctor` checks key presence locally; it does not test access. See the [Decisions API guide](https://developers.openai.com/api/docs/guides/decisions).

Search sends the query, relative paths, source previews, and selected source excerpts to OpenAI. Choose the smallest useful root and inspect `blink files ROOT` first. Filename exclusions do not detect every secret embedded in otherwise eligible source.

## Install the agent skill

Install the [Blink usage skill](skills/blink/SKILL.md) so your coding agent can choose a search scope and interpret returned evidence:

```sh
npx skills add terkesi/blink --skill blink
```

Select your agent and installation scope when prompted. The skill and CLI install separately. The skill does not configure credentials. The repository's verification skill is for developing Blink.

## Search behavior

Blink divides source into windows of at most 80 lines and 4 KiB, with up to eight overlapping lines. Initial source selection combines query terms and paths with exploration across parent directories. Default searches over larger scopes also use up to two requests to score short region previews. Later source selection follows those priorities while reserving one in four windows for the original search order. Thorough mode retains the original source order without preview requests.

Preview scores guide where to read; they never produce result records. Each source request asks up to eight independent relevance questions. A scope that fits one source request and 32 KiB skips previews.

After the initial default pass, accepted source can guide one bounded follow-up pass. Blink selects original source windows using shared identifiers and supplies related accepted source as context. Follow-up judgments can add or retract matches. When that pass finishes, up to two more requests judge windows that declare a function, class or type called by accepted source, with the calling window as context. Keyword and parenthesis patterns find these declarations and calls; this is not a parser or call graph. These requests skip windows that were already planned or accepted, so they can add matches but not retract them, and they skip callers that the follow-up pass rejected. Every multi-window request also asks one listwise question: which candidate, if any, best implements the behavior. A window whose own probability stays below the threshold but which the model names as the batch's best candidate with probability 0.6 or more is nominated. Finally, up to two requests of at most four windows re-judge source that was scored but not accepted, nominated windows first, using all accepted excerpts as shared evidence. When nothing was accepted at all, the strongest nomination is asked alone once instead. Acceptance always requires the window's own probability to reach the threshold; the listwise answer only decides what gets a second look. These judgments can add matches but cannot retract them. Donor source is checked before each request, and accepted follow-up results are checked before output. A deadline preserves already-validated initial results, a deadline during the callee requests preserves the shared-identifier results, and a deadline during the evidence requests preserves the callee results.

Accepted overlapping windows merge into source excerpts. On the first positive judgment for a file, Blink rereads it through the pinned root and compares its hash. That check lets already-validated results survive a later request timeout. Changed or unreadable files are omitted and the operation is incomplete. A later write can still occur after the check.

When accepted records exceed `--limit`, Blink selects the best record from each parent directory before taking another from those directories. Higher probabilities come first within each round. When all records fit, Blink preserves probability order. A helper can be relevant to one step of the query without showing its caller or the entire workflow.

| Limit | Default | `--thorough` |
| --- | --- | --- |
| Search deadline | 15 seconds | 60 seconds |
| HTTP attempts, including retries | 22 | 32 |
| Total encoded request bodies | 768 KiB | 1 MiB |
| Concurrent HTTP requests | 4 | 4 |
| Returned records | 8 | 8 |
| Encoded stdout | 32 KiB | 32 KiB |

`--limit` accepts 1 through 100 records. `--timeout` overrides the deadline with a positive number of seconds, up to 300. Default fresh work is limited to twenty requests and 640 KiB: an initial pass of at most eight requests and 256 KiB, a shared-identifier follow-up within sixteen requests and 512 KiB, up to two callee requests, and up to two evidence requests. The remaining allowance is reserved for retries. Every actual attempt counts against one shared ledger, so an early retry can reduce later fresh work. Each batch, including a preview batch, can retry once after a transient failure within the attempt, byte, and time limits. A window the provider refuses to judge in a batch is asked once more on its own after the current pass finishes its fresh work, when a full attempt timeout remains and, for follow-up windows, its caller is still accepted. That request counts as a retry in the shared ledger, so like any retry it can reduce later fresh work. A second refusal or a failed retry leaves the window unjudged without failing the search. Redirects and automatic HTTP-client retries are disabled. Cancellation drops local requests; it cannot recall work already received by the provider.

JSON schema version 1 includes `results`, `operation`, `coverage`, `budgets`, `errors`, and `output_truncated`. Each result has a relative `path`, the whole-file `sha256`, zero-based byte offsets `start_byte` and exclusive `end_byte`, inclusive one-based line numbers, `probability`, and the exact `excerpt`. Terminal output escapes control characters. JSON preserves the source bytes as decoded UTF-8 text.

`coverage.complete` requires full enumeration, complete planning, and a valid judgment for every eligible window. A bounded search can return useful results with incomplete coverage. Check coverage separately from `operation`. A window the provider refuses to judge is asked once more on its own; a repeated refusal counts as unjudged and is not an error. Output limits keep whole records and report omissions.

The relevance threshold remains provisional at 0.5. At `2560970`, two independently authored synthetic holdouts returned every required source span for 6 of 12 and 16 of 18 positive questions. Queries for absent behavior returned source in 0 of 12 and 5 of 18 cases. Some of those excerpts were tests that contradicted the requested behavior.

Evaluation of three public repositories exposed a larger gap. The initial production version completed 5 of 30 positive questions. The follow-up implementation at `3bf2b5b` completed 15 of 30, returned source on 2 of 15 absent-behavior questions, and recorded three provider refusals across the 45 searches. All reported incomplete coverage. Bounded callee requests, measured at `f5bf830`, raised that diagnostic to 18 of 30 with the same negative-source and refusal questions. This dataset has already informed development, so these results are diagnostic rather than fresh validation. Narrow the search root when possible, inspect returned source, and use other search methods when coverage is incomplete. See the [verification guide](docs/verification.md) for metric definitions and evaluation commands.

Region previews improved a separate calibration set from 4/12 to 7/12 complete positive answers. A bounded related-source pass then reached 10/12 in two runs, with source on 5/12 negative questions and no execution errors. Median requests rose from eight to sixteen. Bounded callee requests then reached 11/12 in two runs at a median of 17.5 to 18 requests. A fresh held-out comparison of 23 questions followed. This version completed 6 of 11 positive questions and the Reference CLI completed 8. Blink returned source on 6 of 12 negative questions and the Reference CLI on 12. Two Blink trials ended with provider refusals, so Blink has not qualified for team release. On a sealed hand-written set authored independently (starlette and bat, 12 multi-span positives and 12 negatives) Blink completed 9 of 12 against the Reference CLI's 11, with 1 against 6 false sources on no-answer questions, 14 against 48 median requests and 1.6 s against 14.7 s median wall time. On 360 generated documentation-derived positives the tools are level (300 against 304) at 18 against 58 median requests; those questions are easier than hand-written ones and support relative comparison only. The [benchmark history](benchmarks/2026-10-08.md) records the comparisons, costs and limits.

## Inventory policy

Blink includes regular UTF-8 text files regardless of extension. Empty files are eligible. NUL bytes and control bytes other than tab, newline, carriage return, and form feed classify a file as binary.

Blink applies root and nested `.gitignore` files. When the selected root is inside a Git repository, Blink also reads ancestor `.gitignore` files up to the nearest `.git` marker. Without a repository marker, rules outside the selected root do not apply. Deeper rules take precedence. Excluded directories are pruned, so a negated descendant cannot restore a file inside an excluded directory.

Root `.blinkignore` uses Git ignore syntax as an additional exclusion layer. Its negations affect that layer only. Nested `.blinkignore`, `.ignore`, `.git/info/exclude`, global Git excludes, and user ignore configuration do not apply. Blink uses `ignore::gitignore` matchers over descriptor-based enumeration, with no `WalkBuilder` defaults or ambient configuration.

Hidden entries are excluded. Ignore negations cannot override hidden filtering or these case-insensitive hard exclusions:

- Metadata names `.git`, `.hg`, and `.svn`.
- Dependency names `node_modules` and `vendor`.
- Build names `target`, `dist`, and `build`.
- Names beginning with `.env`, `id_rsa`, `id_dsa`, `id_ecdsa`, `id_ed25519`, `credentials.`, `service-account`, `service_account`, `client_secret`, `private_key`, `private-key`, `privatekey`, or `ssh_host_`.
- Credential names `.ssh`, `.aws`, `.azure`, `.gcp`, `.gnupg`, `gcloud`, `credentials`, `.credentials`, `secrets`, `.secrets`, `.netrc`, `.npmrc`, `.pypirc`, `.git-credentials`, `.dockercfg`, `.kube`, `.docker`, `.vault`, `.password-store`, `private_keys`, and `private-keys`.
- Extensions `.pem`, `.key`, `.p12`, `.pfx`, `.pkcs12`, `.keystore`, `.jks`, `.ppk`, `.pgp`, `.gpg`, and `.asc`.

These hard names apply to files and directories, including components of an explicitly selected root. No symlink beneath the selected root or special file is eligible. Blink resolves the selected root once, opens each resolved component without following symlinks, and pins that directory descriptor. It enumerates and reopens descendants through that descriptor. It checks every descendant directory component without following symlinks. Canonical paths alone do not authorize source reads. Source files and ignore rules use the same bounded, no-follow reader.

## Limits and coverage

Default ceilings are 1 MiB per file, 64 MiB of reads per run, and 100,000 visited entries. The reusable API can lower these limits. Ignore-file reads and freshness rereads consume the same run byte budget. Files larger than the per-file limit are excluded. Exhausted run budgets stop traversal and mark coverage incomplete. Cancellation and deadline checks are cooperative between entries and read chunks. They cannot interrupt a filesystem syscall already in progress.

JSON schema version 1 includes `files` with `path`, `bytes`, and `sha256`. `coverage` reports visited entries, included files and bytes, actual bytes read, exclusion counts, issues, `stops`, and `complete`. Exclusion counts describe encountered entries. A pruned directory counts once. Its unvisited descendants are not counted. The entry limit can conservatively report incomplete coverage when the last allowed entry was the directory's final entry.

Unreadable entries and unreadable or invalid ignore rules make coverage incomplete. The affected ignore-rule subtree is excluded. File paths are relative to the selected root. Diagnostics outside the selected root use `<ancestor>` instead of an absolute path. Paths that are not valid UTF-8 are excluded.

Returned files are sorted by relative path. Enumeration stops at its bounds before sorting. An incomplete inventory can contain a filesystem-order subset. Each included file owns its captured bytes, digest, and byte offsets of line starts. A trailing newline adds an empty final line. This is a per-file snapshot, not an atomic repository snapshot. Concurrent writes can change files during a run.

| Status | Meaning |
| --- | --- |
| 0 | Results returned with completed execution under the selected policy, or another command completed. |
| 1 | Search found no matches after judging the entire eligible scope. |
| 2 | Invalid invocation or configuration before execution. |
| 3 | Failed or incomplete execution, including an empty search with unjudged source. |
| 130 | Search interrupted by the user. |

A downstream reader closing the output pipe ends the command successfully. Other output failures use status 3. Built-in help and `--version` use Clap's text format.

## Local doctor

`doctor` checks only whether `OPENAI_API_KEY` is present and nonempty. It never prints the value or contacts a provider. Presence does not prove authentication or model access. `files`, `doctor`, and `version` do not need a key.

## Source API

`source::Source::open(root)` pins the selected directory. `Source::snapshot(Limits, &mut dyn FnMut() -> Control)` returns a `Snapshot`. `Control` supports `Continue`, `Cancel`, and `Deadline` without a runtime framework.

`Snapshot::files()` returns immutable `SourceFile` values. Their accessors expose relative `path()`, UTF-8 `text()`, `bytes()`, `sha256()`, and `line_offsets()`. `Snapshot::coverage()` returns the typed coverage record.

`Snapshot::recheck(index, control)` rereads through the original root descriptor and returns `io::Result<bool>`. True means the current digest matches. False means the content or file type changed. Read failures return an error and add a coverage issue. Stops return an interrupted error and append a coverage stop reason. Rereads debit the original cumulative byte budget, so callers must leave enough room for freshness checks. An entry-limit stop permits rereads of captured files while coverage stays incomplete. Byte limits, cancellation, and deadlines prohibit further reads. `coverage.stops` retains every encountered stop reason.

## Verify changes

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
scripts/verify --output /tmp/blink-proof
```

The executable Python helper builds the binary, creates isolated synthetic source, drives all commands, checks source fingerprints, and removes its fixture. It preserves stdout, stderr, exit statuses, and the cleanup result in the selected evidence directory. That directory must be absent or empty. The [verification skill](.agents/skills/verify/SKILL.md) documents the full drive and [feature map](.agents/skills/verify/features/README.md).

See [verification.md](docs/verification.md) for evaluation, preparation benchmarks, arithmetic proofs, fuzzing, and native build checks. Measured results and rejected experiments are recorded in [benchmarks](benchmarks/README.md). Optional [development memory](docs/development-memory.md) records decisions and evidence in a separate local repository.
