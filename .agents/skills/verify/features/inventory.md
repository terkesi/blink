# Source inventory

## Sub-features

`files` lists eligible UTF-8 text from the current working tree. JSON includes relative paths, byte counts, SHA-256 hashes, exclusions, errors, and coverage.

## How to get to it (user POV)

Run `blink files` in the directory to inspect. Pass a root to inspect another directory. Add `--json` before or after `files` for machine-readable output.

## Driving it with the CLI

Run `scripts/verify --output /tmp/blink-proof`. Inspect `files-json.stdout`, `files-terminal.stdout`, and their `.status` files. The fixture must yield `README.md`, `src/app.rs`, and `src/keep.log`. Every returned hash must match fixture bytes. Both source fingerprints must be identical. `missing-root.status` must be 2. `incomplete-policy.status` must be 3.

## Gotchas

Terminal paths are JSON-quoted to escape control characters. Exclusion counts describe encountered files and pruned directories, not descendants of pruned directories. Hidden files remain excluded even when ignore rules negate a pattern. A selected subdirectory inherits repository ignore rules. Invalid or unreadable ignore rules make coverage incomplete and exclude the affected subtree. Files are individual snapshots, not an atomic repository snapshot.
