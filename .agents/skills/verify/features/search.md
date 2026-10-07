# Source search

## Sub-features

`search` returns exact source selected by relevance judgments. It bounds request attempts, encoded bytes, concurrency, elapsed time, and output. Coverage distinguishes unjudged source from no matches.

## How to get to it (user POV)

Set `OPENAI_API_KEY` in the environment. Run `blink search "where are failed requests retried" src --json`. Use `--thorough` for a larger request budget. Use `files src` to inspect eligibility before searching.

## Driving it with the CLI

Run `scripts/verify --output /tmp/blink-proof`. `search-missing-key.status` must be 2. `search-empty.status` must be 1 with complete coverage and zero attempts. Invalid options and missing roots must exit 2. `search-loopback-tests.stdout` must report passing integration tests for the actual coordinator and HTTP boundary. The tests cover initial and follow-up admission, donor and result freshness, late retries within the shared ledger, and observed errors retained when work is cancelled.

For live verification, create a temporary directory containing only synthetic source. Search it with the real credential supplied through the environment. Preserve the JSON, stderr, status, request count, and source hash. Remove only the fixture. A provider error leaves live verification unverified. A live synthetic result proves that case, not held-out retrieval quality.

## Gotchas

`doctor` never tests model access. Tests use a synthetic key and the local provider constructor, with no CLI endpoint override. Refusals are unjudged. Exit 1 requires complete enumeration and judgment. Deadline, output truncation, or quota exhaustion cannot turn an unjudged search into a negative answer. Freshness checks cover individual files and cannot prevent later writes.
