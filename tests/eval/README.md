# Source retrieval corpus

`corpus.json` contains 140 newly authored questions and exact source labels across six synthetic repositories.
Calibration has 30 answerable questions and 10 negative questions across two repositories.
Held-out has 80 answerable questions and 20 negative questions across four repositories.
Every repository has five negative questions and three eligible source files, including a file of unrelated display logic.

The repositories cover parcel handling, venue operations, garden planning, darkroom work, audio editing, and trail navigation.
Calibration and held-out repositories have different source files and function names.
Questions that share a function ask about different behavior and have different required spans.
The snippets are small source fragments. They are not complete applications and do not need to compile.

## Label format

Each question records its repository, split, query, tags, and `expected_spans`.
Each span contains a repository-relative `path`, inclusive 1-based `start_line` and `end_line`, the exact `snippet`, and its UTF-8 SHA-256.
The snippet preserves the source line endings, including the final newline.
A negative question has an empty span list because its requested behavior is absent from eligible source.

Language tags are `python`, `typescript`, and `rust`.
The other tags cover ordinary behavior, vocabulary mismatch, deep directories, multiple files, source after line 80, Unicode, CRLF, instruction text, and negative questions.
`mismatch_terms` records query words absent from the target source text.
The instruction text appears in string literals outside the labeled answer.
It is fixture data and carries no authority over retrieval.

`source_files` lists every eligible fixture file. `excluded_files` lists synthetic sentinels under `scratch/`, which the fixture `.gitignore` excludes, and `node_modules/`, which the retrieval implementation must exclude.
No excluded source range is an answer.
Give the retriever only the repository directory. Keep the question labels and this document outside its source input.

## Metrics

For an answerable question, hit@8 is one when any required span is fully contained in one of the first eight actual returned results from the same repository and path.
Otherwise hit@8 is zero. Average this value over answerable questions only.
Combining incomplete excerpts does not count as containing a span.

All-required-span coverage is one when every required span is fully contained in an actual returned result among the first eight results.
Report its average separately for the questions tagged `multifile`.
Also report the fraction of required spans covered for each multifile question.
A question with one required file does not enter the multifile average.

For negative questions, report the fraction that return no results and the number of returned results.
Negative questions do not enter hit@8.
Validate each returned excerpt against its actual source range and hash before counting a hit.
Labels describe required evidence rather than every window that might reasonably answer a question.

## Validation

Run the standard-library gate from the repository root.

```sh
scripts/check-eval-corpus
python3 -m unittest discover -s tests/eval -p 'test_corpus.py'
```

The gate prints one JSON object with counts and exits with code 0 when the corpus passes.
It exits with code 1 and a JSON error when a label or fixture fails validation.
The mutation tests verify rejection of broken counts, ranges, snippets, hashes, split isolation, path confinement, exclusions, and edge-case tags.

## Limitations

This is a synthetic corpus only. It supports no claim about arbitrary real-world repositories.
The fixtures are compact, the labels share some functions, and distractors are limited.
Question counts do not represent independent production tasks.
The validator checks label structure and exact source correspondence. Human review must still assess semantic relevance and negative completeness.
Symbol and content checks enforce mechanical split isolation. Human review must assess whether examples are conceptually distinct.

No live model evaluation has run against this corpus.
There are no measured retrieval quality scores, chosen relevance thresholds, or mocked-model recall claims.
Use calibration only to choose a threshold. Keep held-out questions separate from tuning, and evaluate them after the threshold is fixed.
