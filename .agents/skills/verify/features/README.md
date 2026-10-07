# Blink feature map

## Sub-features

- [Source inventory](inventory.md) covers files, policy, hashes, coverage, and errors.
- [Source search](search.md) covers exact excerpts, provider requests, budgets, and incomplete results.
- [Local doctor](doctor.md) covers key presence and local readiness.
- [Version](version.md) covers the compiled package version.

## How to get to it (user POV)

Run `blink --help` to list the available commands. Run a subcommand with `--help` for its arguments.

## Driving it with the CLI

Run `scripts/verify --output /tmp/blink-proof` from the repository root. The helper drives all mapped features through the built executable.

## Gotchas

The output directory must be empty or absent. Source fixtures are temporary. Command evidence survives cleanup. Offline search tests use a local server and do not measure model relevance.
