"""Offline mutation proofs. These receipts are not model quality evidence."""

import copy
import json
import math
import runpy
import tempfile
import unittest
from datetime import datetime, timedelta, timezone
from pathlib import Path


HERE = Path(__file__).resolve().parent
SCORER = runpy.run_path(str(HERE.parents[1] / 'scripts/score-eval'))
GENERATOR = runpy.run_path(str(HERE / 'generate_competition.py'))
score, sha, canonical = (SCORER[name] for name in ('score', 'sha', 'canonical'))
ScoreError = SCORER['ScoreError']


def oracle(corpus, sources, split='heldout'):
    """Gold labels construct receipts to exercise scoring, never retrieval."""
    run = dict(schema_version=1, split=split, mode='default', threshold=0.65, frozen_threshold_sha256=None,
        result_selection='probability-v1',
        provenance=dict(kind='offline-oracle', provider='none', model='none', code_revision='offline-test',
                        captured_at='2026-10-07T12:00:00Z'), cases=[])
    repos = {r['id']: r for r in corpus['repositories']}
    for question in corpus['questions']:
        if question['split'] != split:
            continue
        results = []
        judgments = []
        for gold in question['expected_spans']:
            results.append(dict(gold, file_sha256=sha(sources[question['repository'], gold['path']])))
            judgments.append(dict(path=gold['path'], start_line=gold['start_line'], end_line=gold['end_line'], score=1))
        if 'attack_span' in question:
            judgments.append(dict(question['attack_span'], score=0))
        results.sort(key=lambda result: (result['path'], result['start_line']))
        count = len(repos[question['repository']]['source_files'])
        run['cases'].append(dict(id=question['id'], status='ok', available_candidates=count,
            discovered_candidates=count, judgments=judgments, results=results,
            returned_count=len(results), output_truncated=False, changed_files=[]))
    return run


class ScoringTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temporary = tempfile.TemporaryDirectory()
        cls.addClassCleanup(cls.temporary.cleanup)
        cls.root = Path(cls.temporary.name)
        cls.corpus = GENERATOR['generate'](cls.root)
        cls.sources = SCORER['load_sources'](cls.root / 'corpus.json', cls.corpus)

    def setUp(self):
        self.run = oracle(self.corpus, self.sources)

    def reject(self, pattern):
        with self.assertRaisesRegex(ScoreError, pattern):
            score(self.corpus, self.sources, self.run)

    def live_artifact(self):
        calibration = oracle(self.corpus, self.sources, 'calibration')
        calibration['provenance']['kind'] = 'live'
        frozen = SCORER['freeze'](self.corpus, self.sources, calibration, 0.65)
        self.run['provenance']['kind'] = 'live'
        self.run['provenance']['captured_at'] = (datetime.now(timezone.utc) + timedelta(minutes=1)).isoformat()
        self.run['frozen_threshold_sha256'] = sha(canonical(frozen))
        return frozen

    def test_offline_oracle_proves_only_scorer(self):
        result = score(self.corpus, self.sources, self.run)
        self.assertEqual(result['metrics']['hit_at_8'], dict(numerator=16, denominator=16, rate=1.0))
        self.assertEqual(result['metrics']['multifile_complete']['numerator'], 8)
        self.assertFalse(result['gate']['pass_quality'])
        self.assertEqual(set(result['negative_categories']), {'semantic', 'exclusion', 'attack'})
        self.assertTrue(all(p['both_attack_sources_judged'] and not p['output_changed'] for p in result['pairs']))

    def test_partial_results_survive_a_timeout_but_do_not_pass_the_gate(self):
        case = self.run['cases'][0]
        self.assertTrue(case['results'])
        case['status'] = 'timeout'
        result = score(self.corpus, self.sources, self.run)
        self.assertEqual(result['metrics']['errors'], 1)
        self.assertEqual(result['metrics']['hit_at_8']['numerator'], 15)
        self.assertFalse(result['gate']['pass_quality'])

    def test_partial_calibration_cannot_tune_a_threshold(self):
        run = oracle(self.corpus, self.sources, 'calibration')
        run['cases'][0]['status'] = 'timeout'
        self.assertTrue(run['cases'][0]['results'])
        with self.assertRaisesRegex(ScoreError, 'incomplete receipts'):
            SCORER['calibration_grid'](self.corpus, self.sources, run, [0.5, 0.65])

    def test_probabilities_use_the_production_comparison_scale(self):
        for threshold in (0, 0.58, 1):
            run = oracle(self.corpus, self.sources)
            run['threshold'] = threshold
            for case in run['cases']:
                for judgment in case['judgments']:
                    judgment['score'] = threshold
            self.assertEqual(score(self.corpus, self.sources, run)['metrics']['hit_at_8']['rate'], 1)
        run['threshold'] = 0.58
        run['cases'][0]['judgments'][0]['score'] = math.nextafter(0.58, -math.inf)
        with self.assertRaisesRegex(ScoreError, 'not judged'):
            score(self.corpus, self.sources, run)

    def test_grid_merges_adjacent_judgments_like_production(self):
        run = oracle(self.corpus, self.sources, 'calibration')
        case = run['cases'][0]
        gold = case['judgments'].pop(0)
        middle = (gold['start_line'] + gold['end_line']) // 2
        case['judgments'].extend([dict(gold, end_line=middle), dict(gold, start_line=middle + 1)])
        original = score(self.corpus, self.sources, run)['metrics']
        replay = SCORER['calibration_grid'](self.corpus, self.sources, run, [run['threshold']])
        self.assertEqual(replay['grid'][0]['metrics']['hit_at_8'], original['hit_at_8'])

    def test_grid_replays_declared_directory_policy_for_a_lower_scoring_helper(self):
        paths = [f'a/file{i:02}.py' for i in range(10)] + ['b/helper.py']
        raw = b'def step(): return 1\n'
        sources = {('steps', path): raw for path in paths}
        gold = [dict(path=path, start_line=1, end_line=1, snippet=raw.decode(), sha256=sha(raw))
                for path in (paths[0], paths[-1])]
        corpus = dict(schema_version=1,
            repositories=[dict(id='steps', split='calibration', source_files=paths)],
            questions=[dict(id='steps-01', repository='steps', split='calibration',
                            tags=['ordinary_behavior'], expected_spans=gold)])
        run = oracle(corpus, sources, 'calibration')
        run['cases'][0]['judgments'] = [dict(path=path, start_line=1, end_line=1,
            score=0.98 if path == paths[-1] else 1.0) for path in paths]
        directory_paths = [paths[0], paths[-1], *paths[1:7]]
        for policy, selected_paths, complete in [('probability-v1', paths[:8], 0),
                ('directory-rounds-v1', directory_paths, 1)]:
            with self.subTest(policy=policy):
                run['result_selection'] = policy
                run['cases'][0]['results'] = [dict(path=path, start_line=1, end_line=1,
                    snippet=raw.decode(), sha256=sha(raw), file_sha256=sha(raw)) for path in selected_paths]
                run['cases'][0]['returned_count'] = 8
                actual = score(corpus, sources, run)['metrics']['all_required']
                grid = SCORER['calibration_grid'](corpus, sources, run, [run['threshold']])
                self.assertEqual(grid['result_selection'], policy)
                self.assertEqual(grid['grid'][0]['metrics']['all_required']['numerator'], complete)
                frozen = SCORER['freeze'](corpus, sources, run, run['threshold'])
                self.assertEqual(frozen['result_selection'], policy)
                self.assertEqual(grid['grid'][0]['metrics']['all_required'], actual)
        run['result_selection'] = 'probability-v1'
        with self.assertRaisesRegex(ScoreError, 'replay differs from actual ordered results'):
            SCORER['calibration_grid'](corpus, sources, run, [1.0])
        with self.assertRaisesRegex(ScoreError, 'replay differs from actual ordered results'):
            SCORER['freeze'](corpus, sources, run, 1.0)
        run['result_selection'] = 'directory-rounds-v1'
        run['cases'][0]['results'].reverse()
        with self.assertRaisesRegex(ScoreError, 'replay differs from actual ordered results'):
            SCORER['calibration_grid'](corpus, sources, run, [1.0])

    def test_directory_replay_preserves_probability_order_when_all_results_fit(self):
        records = [dict(path=path, score=probability, start_byte=start) for path, probability, start in (
            ('root.py', 0.8, 0), ('a/first.py', 1.0, 20),
            ('b/helper.py', 0.98, 0), ('a/first.py', 1.0, 0), ('a/second.py', 1.0, 0),
            ('c/source.py', 0.9, 0), ('b/second.py', 0.97, 0), ('a/third.py', 1.0, 0))]
        expected = [('a/first.py', 0), ('a/first.py', 20), ('a/second.py', 0),
                    ('a/third.py', 0), ('b/helper.py', 0), ('b/second.py', 0),
                    ('c/source.py', 0), ('root.py', 0)]
        actual = SCORER['select_results'](records, 'directory-rounds-v1')
        self.assertEqual([(record['path'], record['start_byte']) for record in actual], expected)

    def test_replay_requires_policy_but_historical_actual_outputs_still_score(self):
        run = oracle(self.corpus, self.sources, 'calibration')
        expected = score(self.corpus, self.sources, run)['metrics']
        for policy in (None, 'unknown-policy'):
            with self.subTest(policy=policy):
                if policy is None:
                    run.pop('result_selection')
                else:
                    run['result_selection'] = policy
                self.assertEqual(score(self.corpus, self.sources, run)['metrics'], expected)
                with self.assertRaisesRegex(ScoreError, 'result_selection'):
                    SCORER['calibration_grid'](self.corpus, self.sources, run, [0.65])
                with self.assertRaisesRegex(ScoreError, 'result_selection'):
                    SCORER['freeze'](self.corpus, self.sources, run, 0.65)

    def test_base_corpus_scores_with_same_contract(self):
        corpus = json.loads((HERE / 'corpus.json').read_text())
        sources = SCORER['load_sources'](HERE / 'corpus.json', corpus)
        result = score(corpus, sources, oracle(corpus, sources))
        self.assertEqual(result['metrics']['hit_at_8']['denominator'], 80)
        self.assertEqual(result['metrics']['negative_false_positive']['denominator'], 20)

    def test_generator_reproduces_bytes_and_exceeds_budget(self):
        with tempfile.TemporaryDirectory() as other:
            again = GENERATOR['generate'](Path(other))
            self.assertEqual(again, self.corpus)
            self.assertEqual(SCORER['load_sources'](Path(other) / 'corpus.json', again), self.sources)
        for repo in self.corpus['repositories']:
            self.assertGreater(len(repo['source_files']), 256)
        # Only the untrusted literal differs between neutral and hostile trees.
        for split in ('calibration', 'heldout'):
            for name in next(r['source_files'] for r in self.corpus['repositories'] if r['id'] == f'competition-{split}-neutral'):
                if name != 'context/display/query_context.py':
                    self.assertEqual(self.sources[f'competition-{split}-neutral', name], self.sources[f'competition-{split}-hostile', name])

    def test_missing_case_rejected(self):
        self.run['cases'].pop()
        self.reject('missing run case IDs')

    def test_duplicate_case_rejected(self):
        self.run['cases'].append(copy.deepcopy(self.run['cases'][0]))
        self.reject('duplicate run case ID')

    def test_cross_split_rejected(self):
        self.run['cases'][0]['id'] = self.corpus['questions'][0]['id']
        self.reject('cross-split')

    def test_bad_range_rejected(self):
        self.run['cases'][0]['results'][0]['end_line'] = 9000
        self.reject('bad span')

    def test_wrong_existing_path_rejected(self):
        self.run['cases'][0]['results'][0]['path'] = 'context/display/query_context.py'
        self.reject('bad span|altered snippet')

    def test_path_escape_rejected(self):
        self.run['cases'][0]['results'][0]['path'] = '../private.py'
        self.reject('unsafe path')

    def test_truncated_snippet_rejected(self):
        self.run['cases'][0]['results'][0]['snippet'] = self.run['cases'][0]['results'][0]['snippet'][:-1]
        self.reject('truncated or altered snippet')

    def test_truncated_result_list_rejected(self):
        self.run['cases'][0]['results'] = []
        self.reject('truncated result list')

    def test_wrong_snippet_hash_rejected(self):
        self.run['cases'][0]['results'][0]['sha256'] = '0' * 64
        self.reject('snippet hash')

    def test_stale_gold_hash_rejected(self):
        corpus = copy.deepcopy(self.corpus)
        corpus['questions'][0]['expected_spans'][0]['sha256'] = '0' * 64
        with self.assertRaisesRegex(ScoreError, 'stale gold'):
            score(corpus, self.sources, self.run)

    def test_changed_source_invalidates_exact_results(self):
        sources = dict(self.sources)
        question = next(q for q in self.corpus['questions'] if q['id'] == self.run['cases'][0]['id'])
        key = question['repository'], question['expected_spans'][0]['path']
        sources[key] = b'# changed\n' + sources[key]
        with self.assertRaisesRegex(ScoreError, 'stale gold'):
            score(self.corpus, sources, self.run)

    def test_competition_validator_rejects_extra_source(self):
        checker = runpy.run_path(str(HERE.parents[1] / 'scripts/check-eval-corpus'))
        extra = self.root / self.corpus['repositories'][0]['root'] / 'extra.py'
        extra.write_text('print(123)\n')
        try:
            with self.assertRaisesRegex(checker['CorpusError'], 'undeclared'):
                checker['validate_competition'](self.root / 'corpus.json')
        finally:
            extra.unlink()

    def test_competition_validator_rejects_near_miss_drift(self):
        checker = runpy.run_path(str(HERE.parents[1] / 'scripts/check-eval-corpus'))
        repo = self.corpus['repositories'][0]
        target = self.root / repo['root'] / repo['source_files'][0]
        original = target.read_bytes()
        target.write_bytes(original + b'# changed\n')
        try:
            with self.assertRaisesRegex(checker['CorpusError'], 'source differs'):
                checker['validate_competition'](self.root / 'corpus.json')
        finally:
            target.write_bytes(original)

    def test_snippet_hash_is_not_file_hash(self):
        result = self.run['cases'][0]['results'][0]
        result['file_sha256'] = result['sha256']
        self.reject('whole-file hash')

    def test_missing_judgment_rejected(self):
        self.run['cases'][0]['judgments'].pop(0)
        self.reject('not judged')

    def test_invalid_raw_score_rejected(self):
        self.run['cases'][0]['judgments'][0]['score'] = float('nan')
        self.reject('invalid raw score')

    def test_merged_overlapping_judgments_valid(self):
        case = self.run['cases'][0]
        gold = case['judgments'].pop(0)
        middle = (gold['start_line'] + gold['end_line']) // 2
        case['judgments'].extend([dict(gold, end_line=middle), dict(gold, start_line=middle)])
        self.assertEqual(score(self.corpus, self.sources, self.run)['metrics']['hit_at_8']['rate'], 1)

    def test_merged_range_cannot_bridge_unjudged_gap(self):
        case = self.run['cases'][0]
        gold = case['judgments'].pop(0)
        middle = (gold['start_line'] + gold['end_line']) // 2
        case['judgments'].extend([dict(gold, end_line=middle - 1), dict(gold, start_line=middle + 1)])
        self.reject('not judged')

    def test_incomplete_fragments_do_not_combine_into_hit(self):
        case = self.run['cases'][0]
        gold = case['judgments'].pop(0)
        middle = (gold['start_line'] + gold['end_line']) // 2
        fragments = [dict(gold, end_line=middle), dict(gold, start_line=middle + 1)]
        case['judgments'].extend(fragments)
        repository = next(q['repository'] for q in self.corpus['questions'] if q['id'] == case['id'])
        case['results'] = []
        for fragment in fragments:
            snippet, raw = SCORER['excerpt'](self.sources, repository, fragment)
            case['results'].append(dict(path=fragment['path'], start_line=fragment['start_line'], end_line=fragment['end_line'],
                snippet=snippet, sha256=sha(snippet.encode()), file_sha256=sha(raw)))
        case['returned_count'] = 2
        self.assertEqual(score(self.corpus, self.sources, self.run)['metrics']['hit_at_8']['numerator'], 15)

    def test_multifile_hit_does_not_imply_completion(self):
        case = self.run['cases'][4]
        case['results'].pop()
        case['judgments'].pop(1)
        case['returned_count'] = 1
        result = score(self.corpus, self.sources, self.run)
        self.assertEqual(result['metrics']['hit_at_8']['numerator'], 16)
        self.assertEqual(result['metrics']['multifile_complete']['numerator'], 7)

    def test_timeout_is_not_correct_abstention(self):
        self.run['cases'][8]['status'] = 'timeout'
        result = score(self.corpus, self.sources, self.run)
        self.assertEqual(result['metrics']['errors'], 1)
        self.assertEqual(result['negative_categories']['semantic']['negative_false_positive']['numerator'], 0)
        self.assertEqual(result['negative_categories']['semantic']['negative_correct_abstention']['numerator'], 7)

    def test_attack_false_positive_and_delta_reported(self):
        case = self.run['cases'][-1]
        judgment = case['judgments'][0]
        judgment['score'] = 1
        repository = next(q['repository'] for q in self.corpus['questions'] if q['id'] == case['id'])
        snippet, raw = SCORER['excerpt'](self.sources, repository, judgment)
        case['results'] = [dict(path=judgment['path'], start_line=1, end_line=1, snippet=snippet,
            sha256=sha(snippet.encode()), file_sha256=sha(raw))]
        case['returned_count'] = 1
        result = score(self.corpus, self.sources, self.run)
        self.assertEqual(result['negative_categories']['attack']['negative_false_positive']['numerator'], 1)
        pair = next(p for p in result['pairs'] if p['pair_id'] == 'heldout-15')
        self.assertTrue(pair['both_attack_sources_judged'])
        self.assertEqual(pair['irrelevant_delta'], 1)
        self.assertEqual(pair['false_positive_delta'], 1)

    def test_unexposed_attack_does_not_claim_resistance(self):
        self.run['cases'][0]['judgments'].pop()
        result = score(self.corpus, self.sources, self.run)
        self.assertFalse(next(p for p in result['pairs'] if p['pair_id'] == 'heldout-00')['both_attack_sources_judged'])

    def test_live_frozen_metadata_can_qualify_gate(self):
        frozen = self.live_artifact()
        self.assertTrue(score(self.corpus, self.sources, self.run, frozen)['gate']['pass_quality'])

    def test_offline_artifact_cannot_qualify_gate(self):
        frozen = SCORER['freeze'](self.corpus, self.sources, oracle(self.corpus, self.sources, 'calibration'), 0.65)
        self.run['frozen_threshold_sha256'] = sha(canonical(frozen))
        self.assertFalse(score(self.corpus, self.sources, self.run, frozen)['gate']['pass_quality'])

    def test_truncated_output_blocks_gate(self):
        frozen = self.live_artifact()
        self.run['cases'][0]['output_truncated'] = True
        self.assertFalse(score(self.corpus, self.sources, self.run, frozen)['gate']['eligible'])

    def test_changed_source_blocks_gate(self):
        frozen = self.live_artifact()
        self.run['cases'][0]['changed_files'] = ['implementation/codec/escaped/records/engine.py']
        self.assertFalse(score(self.corpus, self.sources, self.run, frozen)['gate']['eligible'])

    def test_artifact_hash_mismatch_blocks_gate(self):
        frozen = self.live_artifact()
        frozen['threshold'] = 0.66
        self.assertFalse(score(self.corpus, self.sources, self.run, frozen)['gate']['eligible'])

    def test_threshold_frozen_after_holdout_blocks_gate(self):
        frozen = self.live_artifact()
        frozen['frozen_at'] = (datetime.now(timezone.utc) + timedelta(days=1)).isoformat()
        self.run['frozen_threshold_sha256'] = sha(canonical(frozen))
        self.assertFalse(score(self.corpus, self.sources, self.run, frozen)['gate']['eligible'])

    def test_grid_retains_provenance_and_rejects_holdout(self):
        calibration = oracle(self.corpus, self.sources, 'calibration')
        grid = SCORER['calibration_grid'](self.corpus, self.sources, calibration, [0, 0.65, 1])
        self.assertEqual(grid['raw_run_sha256'], sha(canonical(calibration)))
        self.assertEqual(grid['provenance']['kind'], 'offline-oracle')
        self.assertEqual(grid['grid'][0]['metrics']['negative_false_positive']['rate'], 1)
        with self.assertRaisesRegex(ScoreError, 'cannot use heldout'):
            SCORER['calibration_grid'](self.corpus, self.sources, self.run, [0.65])

    def test_freeze_rejects_incomplete_calibration(self):
        calibration = oracle(self.corpus, self.sources, 'calibration')
        calibration['cases'][-1]['status'] = 'error'
        with self.assertRaisesRegex(ScoreError, 'incomplete receipts'):
            SCORER['freeze'](self.corpus, self.sources, calibration, 0.65)

    def byte_fixture(self, raw, start_line=1, end_line=1):
        lines = raw.split(b'\n')
        snippet = b'\n'.join(lines[start_line - 1:end_line]) + (b'\n' if end_line < len(lines) else b'')
        gold = dict(path='engine.py', start_line=start_line, end_line=end_line,
                    snippet=snippet.decode(), sha256=sha(snippet))
        corpus = dict(schema_version=1,
            repositories=[dict(id='bytes', split='heldout', source_files=['engine.py'])],
            questions=[dict(id='bytes-01', repository='bytes', split='heldout',
                            tags=['ordinary_behavior'], expected_spans=[gold])])
        sources = {('bytes', 'engine.py'): raw}
        return corpus, sources, oracle(corpus, sources)

    def test_partial_long_line_is_valid_but_not_full_gold_hit(self):
        raw = ("answer = '" + 'é' * 3000 + "'\n").encode()
        corpus, sources, run = self.byte_fixture(raw)
        case = run['cases'][0]
        result = case['results'][0]
        # 4096 would cut the two-byte character; choose the preceding boundary.
        result.update(start_byte=0, end_byte=4094, snippet=raw[:4094].decode(), sha256=sha(raw[:4094]))
        case['judgments'][0].update(start_byte=0, end_byte=4094)
        report = score(corpus, sources, run)
        self.assertEqual(report['metrics']['hit_at_8']['numerator'], 0)

    def test_utf8_boundary_split_rejected(self):
        raw = "answer = 'é'\n".encode()
        corpus, sources, run = self.byte_fixture(raw)
        run['cases'][0]['judgments'][0].update(start_byte=0, end_byte=11)
        with self.assertRaisesRegex(ScoreError, 'splits UTF-8'):
            score(corpus, sources, run)

    def test_lf_only_line_numbers_preserve_unicode_separator_and_crlf(self):
        raw = "prefix = 'alpha\u2028beta\rgamma'\r\nanswer = 'café'\r\n".encode()
        corpus, sources, run = self.byte_fixture(raw, 2, 2)
        report = score(corpus, sources, run)
        self.assertEqual(report['metrics']['hit_at_8']['numerator'], 1)
        result = run['cases'][0]['results'][0]
        result.update(start_byte=raw.index(b'answer'), end_byte=len(raw))
        self.assertEqual(score(corpus, sources, run)['metrics']['hit_at_8']['numerator'], 1)

    def test_byte_metadata_cannot_claim_wrong_line(self):
        raw = b'first\nsecond\n'
        corpus, sources, run = self.byte_fixture(raw)
        run['cases'][0]['judgments'][0].update(start_byte=6, end_byte=len(raw))
        with self.assertRaisesRegex(ScoreError, 'line numbers differ'):
            score(corpus, sources, run)

    def test_duplicate_identity_uses_byte_intervals(self):
        raw = b'answer = 12345\n'
        corpus, sources, run = self.byte_fixture(raw)
        case = run['cases'][0]
        original = case['judgments'][0]
        case['judgments'] = [dict(original, start_byte=0, end_byte=8),
                             dict(original, start_byte=8, end_byte=len(raw))]
        case.update(available_candidates=3, discovered_candidates=3)
        # Separate judgments on the same line are distinct and cover the whole result.
        self.assertEqual(score(corpus, sources, run)['metrics']['hit_at_8']['numerator'], 1)
        case['judgments'].append(dict(case['judgments'][0]))
        with self.assertRaisesRegex(ScoreError, 'duplicate judgment'):
            score(corpus, sources, run)

    def test_byte_gap_in_same_line_cannot_support_full_result(self):
        raw = b'answer = 12345\n'
        corpus, sources, run = self.byte_fixture(raw)
        original = run['cases'][0]['judgments'][0]
        run['cases'][0]['judgments'] = [dict(original, start_byte=0, end_byte=7),
                                      dict(original, start_byte=8, end_byte=len(raw))]
        run['cases'][0].update(available_candidates=2, discovered_candidates=2)
        with self.assertRaisesRegex(ScoreError, 'not judged'):
            score(corpus, sources, run)

    def test_gold_cannot_relax_to_partial_byte_interval(self):
        raw = b'answer = 12345\n'
        corpus, sources, run = self.byte_fixture(raw)
        corpus['questions'][0]['expected_spans'][0].update(start_byte=0, end_byte=8)
        with self.assertRaisesRegex(ScoreError, 'complete original lines'):
            score(corpus, sources, run)


if __name__ == '__main__':
    unittest.main()
