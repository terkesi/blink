# Local doctor

## Sub-features

`doctor` reports the compiled version, inventory readiness, and whether `OPENAI_API_KEY` is nonempty. It never reports the key value.

## How to get to it (user POV)

Run `blink doctor` or `blink doctor --json` from any directory.

## Driving it with the CLI

Run `scripts/verify --output /tmp/blink-proof`. Inspect `doctor-absent.stdout`, `doctor-present.stdout`, and `doctor-terminal.stdout`. All statuses must be 0. Key presence must change between absent and synthetic values. The synthetic value must never appear in captured output.

## Gotchas

Key presence does not establish authentication, permission, or provider availability. Doctor makes no provider request. No key is needed to inventory files.
