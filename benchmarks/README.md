# Benchmarks

[2026-10-08 results](2026-10-08.md) report the first fresh head-to-head comparison and the first generated-set comparison. On 11 held-out hand-written questions Blink completed 6 and the Reference CLI 8; on the three latest generated sets at the current budget Blink completed 311 of 360 against the Reference CLI's 298; on five sealed hand-written sets 47 of 60 against 43 with the current default search (the former thorough mode), with 12 against 42 no-answer sources; on two sealed hand-written sets authored independently Blink completed 10 of 12 and 4 of 12 against the Reference CLI's 11 and 9, with 1 against 6 and 1 against 4 false sources on no-answer questions, at a quarter to a fifth of the requests; on 360 generated positives the tools are level (300 against 304) at a third of the requests, a quarter of the bytes and a tenth of the wall time, with 5 against 16 false sources on 120 no-answer questions. An evidence pass that re-judges rejected windows beside the accepted excerpts added 6 of 480 with no losses, and a listwise nomination question added 17 of 480 with one loss. Blink has not qualified for team release. Bounded callee requests raised calibration from 10/12 to 11/12, and provider refusals no longer fail a search. [2026-10-07 results](2026-10-07.md) record the earlier experiment history.

## Read the metrics

- **Complete positives** count questions for which the tool returns every required source span without an execution or integrity error. The denominator includes every positive question, including failed runs.
- **Source on negatives** counts absent-behavior questions that return any source. A returned counterexample can help an agent answer correctly, so this is not an agent-answer error rate.
- **Errors** count trials with provider, execution, or integrity failures. Incomplete exploration and output truncation are reported separately.
- **Work** records actual requests and encoded request bytes. Different models and providers make request counts an unreliable proxy for price.
- **Time** measures the process end to end. A median across different questions in one quality run is descriptive. It does not replace repeated timing measurements of the same workload.

A complete answer does not require exhaustive repository coverage. Conversely, an empty result with incomplete coverage does not prove that a behavior is absent. None of these metrics establishes downstream coding-task success.

## What each question family is for

- **Sealed hand-written sets** (v3 starlette and bat, v4 jinja and just, v5 typer and jiff, v6 click and fd, v7 hono and zod) are the only evidence that counts towards a release claim. Each is authored independently, with the labels unread by the tool's author until the run, and is consumed by that run: once it has steered a decision it is a diagnostic set, and a release claim needs a fresh one.
- **Generated sets** (werkzeug and ripgrep, questions derived from documentation) are a regression check and nothing more. Both tools are above 80% on them and the remaining misses are mostly weak labels. A change must not lose on them; gaining on them says little, because their misses do not resemble the hand-written ones.
- **Diagnostic probes** (single requests against consumed sets) decide what to build next and are never cited as results.

## The gate

The original gate asked for at least 90% complete hand-written positives, strictly more complete positives than the Reference CLI, no more no-answer sources than the baseline, zero error trials, and repeated cost measurements, all on a fresh sealed set. Five sealed sets in, neither tool reaches 90% on hand-written multi-span questions (the Reference CLI stands at 43 of 60, Blink at 47 of 60 with the current default search), so the first clause does not separate the tools. The gate used from here, on a fresh sealed set with both tools at the same output limit:

1. Blink completes at least as many positives as the Reference CLI, question for question (paired wins at least losses), and at least as many required spans.
2. Blink returns source on no more no-answer questions than the Reference CLI.
3. Zero error trials for Blink.
4. Median requests, bytes and wall time at most half the Reference CLI's, measured in the same run.

On the fourth set default mode met clauses 1 to 3 and clause 4 on wall time only. On the fifth set thorough mode met clauses 1 and 2 (9 against 7, 38 against 37 spans, 4 against 11 no-answer sources), failed clause 3 on one provider 503, and met clause 4 on wall time only; default mode failed clause 1 there.

## Add a run

1. Freeze the source revision, executable hash, corpus, scorer, settings, and acceptance rule before reading results.
2. Label each dataset as calibration, fresh validation, or consumed diagnostic data. Once results inform a change, that dataset cannot provide fresh validation for it.
3. Save commands, raw output, source hashes, errors, and actual work. Preserve failed runs and control builds.
4. Report accuracy alongside resource use. Compare a new mechanism with a control that isolates its contribution.
5. Add a dated report with a keep or reject decision. Run repeated timing comparisons only after quality qualifies.

Keep credentials and source excerpts out of these reports. The current full receipt archive is local and external to this repository. Each report identifies its inputs and hashes; a fresh clone alone cannot reproduce those external datasets.
