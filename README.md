# Blink

Blink inventories eligible source files in your current working tree. This stage provides `files`, `doctor`, and `version`. It does not perform semantic search.

## Run the CLI

Install Rust 1.93.0 through rustup. The repository pins that toolchain and commits `Cargo.lock`. The supported development platforms are macOS and Linux.

```sh
cargo build --locked
./target/debug/blink files .
./target/debug/blink files . --json
./target/debug/blink doctor --json
./target/debug/blink version --json
```

`files` defaults to the current directory. Terminal output contains one JSON-quoted relative path per line. Quoting makes embedded control characters visible. Coverage summaries go to stderr. `--json` works before or after each subcommand and writes one JSON object to stdout. Diagnostics stay on stderr.

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

Unreadable entries and unreadable or invalid ignore rules make coverage incomplete. The affected ignore-rule subtree is excluded. File paths are relative to the selected root. Ignore diagnostics may name an absolute ancestor rule path. Paths that are not valid UTF-8 are excluded.

Returned files are sorted by relative path. Enumeration stops at its bounds before sorting. An incomplete inventory can contain a filesystem-order subset. Each included file owns its captured bytes, digest, and byte offsets of line starts. A trailing newline adds an empty final line. This is a per-file snapshot, not an atomic repository snapshot. Concurrent writes can change files during a run.

Exit status 0 means the command completed. A downstream reader closing the output pipe also ends the command successfully. Status 2 means an invocation or root-open failure. Status 3 means incomplete inventory or another output failure. Status 1 is reserved. Built-in help and `--version` use Clap's text format.

## Local doctor

`doctor` checks only whether `OPENAI_API_KEY` is present and nonempty. It never prints the value or contacts a provider. Presence does not prove authentication or model access. None of the current commands needs a key.

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
