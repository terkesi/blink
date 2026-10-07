---
name: verify
description: Build and drive the Blink CLI through files, doctor, and version. Use after CLI or source-policy changes and before reporting behavior as verified.
---

# Verify Blink

## Launch

Run `cargo build --locked` from the repository root. The binary is `target/debug/blink` unless `CARGO_TARGET_DIR` changes the build directory. `scripts/verify` reads Cargo's build output to locate the executable. A successful build and `blink version` establish that the CLI is ready. Each command exits on its own. No server or teardown command is needed.

## Doctor

Run `target/debug/blink doctor --json`. Expect schema version 1, the Cargo package version, `inventory_ready: true`, and `provider_contacted: false`. The `OPENAI_API_KEY.present` field describes a nonempty environment variable. It does not establish authentication or model access. No key is required for these commands.

## Drive

Run the executable helper from the repository root:

```sh
scripts/verify --output /tmp/blink-proof
```

Choose an absent or empty evidence directory. The helper builds the real binary and creates isolated temporary source. It drives every feature in [the map](features/README.md). It checks ignore rules, sensitive paths, symlinks, binary data, oversized files, hashes, statuses, and sorted JSON and terminal output. It also drives doctor with absent and synthetic keys, version, a missing root, and invalid ignore rules.

## Evidence

The requested output directory contains every command's stdout, stderr, and exit status. `commands.json` records the arguments. `source-before.json` and `source-after.json` record source hashes, modification times, permissions, and symlink targets. `summary.json` records the result and cleanup.

The helper exercises the public binary without replacing internal state or mocking filesystem calls. File hashes and both fingerprints establish the source and absence of writes. The doctor output reports no provider call. The helper does not test provider authentication or search, and does not capture network traffic.

## Cleanup

The helper removes only its own fixture directory in a `finally` block. It starts no background process. Evidence stays in the output directory after successful and failed drives. Check `fixture_removed` in `summary.json` and confirm `files-json.stdout` still exists. Preserve the output directory for review.

## Helpers

`scripts/verify` requires Python 3, Cargo, and the pinned Rust toolchain. Its `--output` option selects the evidence directory. Without it, the helper creates a unique directory under `artifacts/verification/`. Existing evidence directories must be empty. The helper never deletes earlier evidence. Use `/maintain-verification-skill` when the command map changes.
