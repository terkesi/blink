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

Search sends the query, relative paths, and selected source excerpts to OpenAI. Choose the smallest useful root and inspect `blink files ROOT` first. Filename exclusions do not detect every secret embedded in otherwise eligible source.

## Install the agent skill

Install the [Blink usage skill](skills/blink/SKILL.md) so your coding agent can choose a search scope and interpret returned evidence:

```sh
npx skills add terkesi/blink --skill blink
```

Select your agent and installation scope when prompted. The skill and CLI install separately. The skill does not configure credentials. The repository's verification skill is for developing Blink.

## Search behavior

Blink divides source into windows of at most 80 lines and 4 KiB, with up to eight overlapping lines. Half of candidate selection explores files in deterministic order. When the candidate budget cannot cover every window, exploration rotates among parent directories. The other half uses query terms and paths to rank candidates. Each request asks up to eight independent relevance questions.

Accepted overlapping windows merge into source excerpts. On the first positive judgment for a file, Blink rereads it through the pinned root and compares its hash. That check lets already-validated results survive a later request timeout. Changed or unreadable files are omitted and the operation is incomplete. A later write can still occur after the check.

When accepted records exceed `--limit`, Blink selects the best record from each parent directory before taking another from those directories. Higher probabilities come first within each round. When all records fit, Blink preserves probability order. A helper can be relevant to one step of the query without showing its caller or the entire workflow.

| Limit | Default | `--thorough` |
| --- | --- | --- |
| Search deadline | 15 seconds | 60 seconds |
| HTTP attempts, including retries | 8 | 32 |
| Total encoded request bodies | 256 KiB | 1 MiB |
| Concurrent HTTP requests | 4 | 4 |
| Returned records | 8 | 8 |
| Encoded stdout | 32 KiB | 32 KiB |

`--limit` accepts 1 through 100 records. `--timeout` overrides the deadline with a positive number of seconds, up to 300. Each batch can retry once after a transient failure, within the same attempt, byte, and time limits. Redirects and automatic HTTP-client retries are disabled. Cancellation drops local requests; it cannot recall work already received by the provider.

JSON schema version 1 includes `results`, `operation`, `coverage`, `budgets`, `errors`, and `output_truncated`. Each result has a relative `path`, the whole-file `sha256`, zero-based byte offsets `start_byte` and exclusive `end_byte`, inclusive one-based line numbers, `probability`, and the exact `excerpt`. Terminal output escapes control characters. JSON preserves the source bytes as decoded UTF-8 text.

`coverage.complete` requires full enumeration, complete planning, and a valid judgment for every eligible window. A bounded search can return useful results with incomplete coverage. Check coverage separately from `operation`. Refusals count as unjudged. Output limits keep whole records and report omissions.

The relevance threshold remains provisional at 0.5. At `2560970`, two independently authored synthetic holdouts returned every required source span for 6 of 12 and 16 of 18 positive questions. Queries for absent behavior returned source in 0 of 12 and 5 of 18 cases. Some of those excerpts were tests that contradicted the requested behavior.

A subsequent evaluation of three public repositories exposed a larger gap. With the same production source at `afe8d19`, default search returned every required span without execution errors for 5 of 30 positive questions. It returned source for none of 15 absent-behavior questions. Three of the 45 searches had execution errors, and all reported incomplete coverage. These are single trials per question, not an estimate of general accuracy. Narrow the search root when possible, inspect returned source, and use other search methods when coverage is incomplete. See the [verification guide](docs/verification.md) for metric definitions and evaluation commands.

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

See [verification.md](docs/verification.md) for evaluation, preparation benchmarks, arithmetic proofs, fuzzing, and native build checks. Optional [development memory](docs/development-memory.md) records decisions and evidence in a separate local repository.
