"""Offline tests for scripts/paired-summary using synthetic trial dirs."""
import importlib.machinery
import importlib.util
import io
import json
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
LOADER = importlib.machinery.SourceFileLoader('paired_summary', str(REPO / 'scripts' / 'paired-summary'))
SPEC = importlib.util.spec_from_loader('paired_summary', LOADER)
ps = importlib.util.module_from_spec(SPEC)
LOADER.exec_module(ps)


def trial(qid, tool, strict_hit, covered, required, repeat=1,
          error=False, leads=None, requests=3, wall=1.5):
    return dict(
        id=qid, tool=tool, mode='default', repeat=repeat,
        metrics=dict(positive=True, strict_hit_at_8=strict_hit,
                     strict_complete_eligible=strict_hit,
                     required_spans=required, covered_spans=covered,
                     lead_path_hit_at_8=bool(leads),
                     negative_source_false_positive=False,
                     execution_or_integrity_error=error),
        parsed=dict(leads=leads or [], requests=requests,
                    user_output_bytes=100, encoded_request_bytes=900),
        process=dict(wall_seconds=wall))


def negative(qid, tool, records):
    return dict(
        id=qid, tool=tool, mode='default', repeat=1,
        metrics=dict(positive=False, strict_hit_at_8=False,
                     strict_complete_eligible=True, required_spans=0,
                     covered_spans=0, lead_path_hit_at_8=False,
                     negative_source_false_positive=records > 0,
                     execution_or_integrity_error=False),
        parsed=dict(leads=[], requests=1, user_output_bytes=10,
                    encoded_request_bytes=100),
        process=dict(wall_seconds=0.5))


class PairedSummaryTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name) / 'run'

    def tearDown(self):
        self.tmp.cleanup()

    def write(self, name, data):
        d = self.dir / name
        d.mkdir(parents=True)
        (d / 'trial.json').write_text(json.dumps(data))

    def run_summary(self):
        buf = io.StringIO()
        with redirect_stdout(buf):
            self.assertEqual(ps.main([str(self.dir)]), 0)
        return json.loads(buf.getvalue())

    def test_per_tool_and_pairing(self):
        # q1: blink wins on strict; q2: tie; q3: jg wins on recall only.
        self.write('01-b', trial('q1', 'blink', True, 2, 2))
        self.write('01-j', trial('q1', 'jg', False, 1, 2))
        self.write('02-b', trial('q2', 'blink', True, 1, 1))
        self.write('02-j', trial('q2', 'jg', True, 1, 1))
        self.write('03-b', trial('q3', 'blink', True, 1, 2))
        self.write('03-j', trial('q3', 'jg', True, 2, 2))
        self.write('04-b', negative('n1', 'blink', 0))
        self.write('05-j', negative('n1', 'jg', 2))
        report = self.run_summary()
        blink = report['per_tool']['blink/default']
        self.assertEqual(blink['complete_positives'], 3)
        self.assertEqual(blink['span_recall'], dict(covered=4, required=5, rate=0.8))
        self.assertEqual(blink['negative_source_records'], 0)
        self.assertEqual(blink['median_requests'], 3)
        jg = report['per_tool']['jg/default']
        self.assertEqual(jg['negative_source_records'], 1)
        pair = report['paired']
        self.assertEqual(pair['strict_hit_at_8']['wins'], 1)
        self.assertEqual(pair['strict_hit_at_8']['ties'], 2)
        self.assertEqual(pair['span_recall']['wins'], 1)
        self.assertEqual(pair['span_recall']['losses'], 1)
        self.assertEqual(pair['span_recall']['ties'], 1)
        # One win and one loss: n=2, min=1 -> p = 2 * (C(2,0)+C(2,1))/4 = 1.0.
        self.assertEqual(pair['span_recall']['sign_test_p'], 1.0)
        # Strict: one win, zero losses -> p = 2 * 1/2 = 1.0.
        self.assertEqual(pair['strict_hit_at_8']['sign_test_p'], 1.0)

    def test_error_trials_skipped_in_pairs(self):
        self.write('01-b', trial('q1', 'blink', True, 1, 1, error=True))
        self.write('01-j', trial('q1', 'jg', False, 0, 1))
        self.write('02-b', trial('q2', 'blink', False, 0, 1))
        self.write('02-j', trial('q2', 'jg', True, 1, 1))
        report = self.run_summary()
        self.assertEqual(report['per_tool']['blink/default']['error_trials'], 1)
        pair = report['paired']['strict_hit_at_8']
        self.assertEqual(pair['skipped_errors'], 1)
        self.assertEqual(pair['losses'], 1)
        # Single decisive pair: p = 2 * C(1,0)/2 = 1.0.
        self.assertEqual(pair['sign_test_p'], 1.0)


if __name__ == '__main__':
    unittest.main()
