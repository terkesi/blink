# Benchmarks

[2026-10-08 results](2026-10-08.md) report the first fresh head-to-head comparison and the first generated-set comparison. On 11 held-out hand-written questions Blink completed 6 and the Reference CLI 8; on 120 generated questions Blink completed 98 and the Reference CLI 103 (paired p 0.44) at a third of the requests and 7.6 times lower median wall time. Blink has not qualified for team release. Bounded callee requests raised calibration from 10/12 to 11/12, and provider refusals no longer fail a search. [2026-10-07 results](2026-10-07.md) record the earlier experiment history.

## Read the metrics

- **Complete positives** count questions for which the tool returns every required source span without an execution or integrity error. The denominator includes every positive question, including failed runs.
- **Source on negatives** counts absent-behavior questions that return any source. A returned counterexample can help an agent answer correctly, so this is not an agent-answer error rate.
- **Errors** count trials with provider, execution, or integrity failures. Incomplete exploration and output truncation are reported separately.
- **Work** records actual requests and encoded request bytes. Different models and providers make request counts an unreliable proxy for price.
- **Time** measures the process end to end. A median across different questions in one quality run is descriptive. It does not replace repeated timing measurements of the same workload.

A complete answer does not require exhaustive repository coverage. Conversely, an empty result with incomplete coverage does not prove that a behavior is absent. None of these metrics establishes downstream coding-task success.

## Add a run

1. Freeze the source revision, executable hash, corpus, scorer, settings, and acceptance rule before reading results.
2. Label each dataset as calibration, fresh validation, or consumed diagnostic data. Once results inform a change, that dataset cannot provide fresh validation for it.
3. Save commands, raw output, source hashes, errors, and actual work. Preserve failed runs and control builds.
4. Report accuracy alongside resource use. Compare a new mechanism with a control that isolates its contribution.
5. Add a dated report with a keep or reject decision. Run repeated timing comparisons only after quality qualifies.

Keep credentials and source excerpts out of these reports. The current full receipt archive is local and external to this repository. Each report identifies its inputs and hashes; a fresh clone alone cannot reproduce those external datasets.
