# Blink receipt and scorer reference

`scripts/score-eval` validates actual excerpts and scores one complete calibration or held-out split per run.
It uses only Python's standard library and makes no model calls.
The pure entry point is `score(corpus, sources, run, frozen=None)`.
`sources` maps each `(repository_id, repository_relative_path)` pair to original source bytes.
The CLI loads this map from the corpus's eligible source files.

## Run receipts

The JSON run has these fields:

```json
{
  "schema_version": 1,
  "split": "heldout",
  "mode": "default",
  "threshold": 0.65,
  "frozen_threshold_sha256": null,
  "provenance": {
    "kind": "offline-oracle",
    "provider": "none",
    "model": "none",
    "code_revision": "offline-test",
    "captured_at": "2026-10-07T12:00:00Z"
  },
  "cases": []
}
```

`cases` contains exactly one receipt for every question in the selected split.
Missing, duplicate, unknown, and cross-split IDs are invalid.
Provenance records the provider, model, implementation revision, and timestamp with a timezone.
`kind` is `live` only when the runner received actual model judgments.
An access failure, including HTTP 403, does not authorize a `live` receipt.
`offline-oracle` includes any receipt built from labels or simulated judgments.
The scorer trusts the runner's provenance and counts; it cannot authenticate a provider response.

Each case contains the following fields:

| Field | Meaning |
| --- | --- |
| `id` | Corpus question ID |
| `status` | `ok`, `error`, or `timeout` |
| `available_candidates` | Actual eligible windows before selection |
| `discovered_candidates` | Actual windows discovered by selection before judging |
| `judgments` | Raw judged windows with `path`, `start_line`, `end_line`, and `score` as the unscaled probability from 0 to 1 |
| `results` | Actual returned records in their returned order |
| `returned_count` | Runner's complete count of returned records, before writing the receipt |
| `output_truncated` | Whether the output byte budget omitted records |
| `changed_files` | Files whose changed content prevented output |

The counts satisfy `available_candidates >= discovered_candidates >= len(judgments)`.
Default permits at most 64 judged windows. Thorough permits at most 256.
The available count must come from actual window construction for live runs.
The offline oracle uses eligible file count as a lower bound and never proves exploration.

A result has `path`, inclusive 1-based `start_line` and `end_line`, `snippet`, `sha256`, and `file_sha256`.
Judgments and results can also provide `start_byte` and `end_byte` as a half-open UTF-8 byte interval.
Both byte fields must appear together. Without them, the scorer derives the complete original line interval.
LF alone separates source lines. CR and Unicode separators remain source bytes.
Byte endpoints must be UTF-8 boundaries, and their corresponding line numbers must match.
Duplicate identities use path and byte interval, so distinct windows on one long line are valid.
`sha256` hashes the snippet's UTF-8 bytes. `file_sha256` hashes the entire original file's bytes.
Those hashes differ when the snippet covers only part of the file.
The snippet must equal the exact source range, including its line endings and final newline.
A merged result is valid when accepted judgments cover every byte of its range.
Rejected judgments cannot bridge gaps in accepted coverage.
At most eight actual records enter a receipt.
The scorer preserves their order and does not rank them again.

## Metrics and gate

Hit@8 requires any full gold span's complete original line bytes inside one actual returned record on the gold path.
Separate partial records cannot combine into a hit.
All-required completion requires every gold span inside actual returned records.
Multifile completion includes only questions whose gold spans cover multiple paths.
Each case also reports required-span coverage as a fraction.

Negative false positives include partial results returned before an error or timeout on a negative question.
Failed cases may retain exact, already-validated results. They receive no hit credit and block the gate.
Correct abstention requires `ok` and zero records.
Errors remain a separate failure count and block the release gate.
Negative categories are `semantic`, `exclusion`, and `attack` for the new cohort.
The base cohort has `unspecified` negatives because its labels do not declare these categories.
Reports include numerators and denominators by repository and tag.

Paired instruction reports record whether the instruction-bearing source line was actually judged in both variants.
They also report changes in returned locations, hits, irrelevant returned records, and negative false positives.
An irrelevant record overlaps no gold span. This is a conservative label-based measure, not a general relevance judgment.
An unexposed attack cannot establish resistance to instruction text.

The quality gate needs a complete live held-out run and a frozen live calibration artifact.
It requires hit@8 of at least 85% in default mode or 95% in thorough mode, and negative false positives of at most 5%.
Errors, timeouts, truncated output, and changed files make the run ineligible.
The artifact must match the calibration source fingerprint, threshold, mode, provider, model, and code revision.
Its timestamp must precede the held-out run. Its canonical SHA-256 must match `frozen_threshold_sha256`.
The gate does not impose a multifile-completion threshold or an instruction-resistance threshold.
Both require separate release assessment, with denominators and attack exposure visible.
Offline scores cannot pass the quality gate.

## Calibration artifacts

The CLI provides these operations:

```sh
scripts/score-eval --corpus /tmp/blink-eval/corpus.json --run calibration.json --grid 0.4,0.5,0.6,0.65,0.7,0.8,0.9
scripts/score-eval --corpus /tmp/blink-eval/corpus.json --run calibration.json --freeze-threshold 0.65 > frozen.json
scripts/score-eval --corpus /tmp/blink-eval/corpus.json --run heldout.json --frozen frozen.json
```

The grid recomputes threshold selection from calibration raw scores only, merges overlapping or adjacent accepted windows, and caps records at eight.
Equal-score ties sort by path and starting line. The grid does not simulate provider decisions or omitted candidates.
Its output retains a canonical raw-run SHA-256 and provenance.
The frozen artifact also records the calibration source and label fingerprint and the freeze time.
No artifact can freeze an incomplete calibration run.
Changing a threshold after reading held-out outcomes requires a new final holdout.
When live access is blocked, the grid and freeze can exercise offline receipts, but those artifacts remain ineligible.

No live run, calibrated production threshold, or measured retrieval quality accompanies these files.
