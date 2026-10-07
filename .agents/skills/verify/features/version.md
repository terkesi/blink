# Version

## Sub-features

`version` prints the version compiled from Cargo package metadata. JSON also reports schema version 1.

## How to get to it (user POV)

Run `blink version` or `blink version --json`. The top-level `blink --version` flag is also available.

## Driving it with the CLI

Run `scripts/verify --output /tmp/blink-proof`. Inspect `version-json.stdout` and `version-terminal.stdout`. Both must match the `version` field in `Cargo.toml`, and both statuses must be 0.

## Gotchas

A previously built binary retains its compiled version after edits to `Cargo.toml`. Rebuild before verification. `--json` applies to subcommands, not Clap's built-in help and version flags.
