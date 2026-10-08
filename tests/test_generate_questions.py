"""Offline tests for scripts/generate-questions using fixture repositories."""
import hashlib
import importlib.machinery
import importlib.util
import json
import os
import stat
import sys
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
LOADER = importlib.machinery.SourceFileLoader('generate_questions', str(REPO / 'scripts' / 'generate-questions'))
SPEC = importlib.util.spec_from_loader('generate_questions', LOADER)
gq = importlib.util.module_from_spec(SPEC)
LOADER.exec_module(gq)

PY_SOURCE = '''"""Module doc."""


def fetch_widget(session, widget_id):
    """Fetch the widget row for the given session and return it."""
    row = session.query(Widget).get(widget_id)
    if row is None:
        raise LookupError("widget")
    return decorate(row)


def decorate(row):
    """Decorate a database row with display labels for the admin panel."""
    row.label = build_label(row)
    row.extra = compute_extra(row)
    return row


class WidgetStore:
    """Store widgets keyed by their slug for later lookup."""

    def put_widget(self, widget):
        self.items[widget.slug] = widget
        return widget
'''

RS_SOURCE = '''/// Compute the rolling checksum for the buffered stream bytes.
pub fn rolling_checksum(buffer: &[u8]) -> u64 {
    let mut sum = 0u64;
    for byte in buffer {
        sum = sum.wrapping_add(*byte as u64);
    }
    sum
}

fn main() {
    let _ = rolling_checksum(&[1, 2, 3]);
}
'''

TS_SOURCE = '''/**
 * Render the account table with one row per active subscription.
 */
export function renderAccountTable(accounts: Account[]): string {
    return accounts.map((a) => rowFor(a)).join("\\n");
}
'''

OTHER_PY = '''def harvest_field(records, field_name):
    """Harvest the named field from every record and collect the values."""
    out = []
    for record in records:
        out.append(record.get(field_name))
    return out
'''


def write_repo(root, files):
    for rel, text in files.items():
        path = root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)


def fake_blink(directory):
    """A stand-in executable that lists all files like `blink files --json`."""
    script = directory / 'fake-blink'
    script.write_text(
        '#!/usr/bin/env python3\n'
        'import json, sys\n'
        'from pathlib import Path\n'
        'root = Path(sys.argv[2])\n'
        'files = [p.relative_to(root).as_posix() for p in sorted(root.rglob("*")) if p.is_file()]\n'
        'print(json.dumps({"files": [{"path": f} for f in files]}))\n')
    script.chmod(script.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    return script


class GenerateQuestionsTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        base = Path(self.tmp.name)
        self.alpha = base / 'alpha'
        self.beta = base / 'beta'
        write_repo(self.alpha, {'lib/widget.py': PY_SOURCE, 'src/util.rs': RS_SOURCE,
                                'web/table.ts': TS_SOURCE})
        write_repo(self.beta, {'lib/harvest.py': OTHER_PY})
        self.blink = fake_blink(base)
        self.base = base

    def tearDown(self):
        self.tmp.cleanup()

    def run_gen(self, seed, output='corpus.json'):
        out = self.base / output
        argv = ['--root', str(self.alpha), '--id', 'alpha',
                '--root', str(self.beta), '--id', 'beta',
                '--seed', str(seed), '--count', '3', '--negatives', '2',
                '--output', str(out), '--blink', str(self.blink)]
        self.assertEqual(gq.main(argv), 0)
        return json.loads(out.read_text())

    def test_deterministic_same_seed(self):
        a = self.run_gen(7, 'a.json')
        b = self.run_gen(7, 'b.json')
        self.assertEqual(a['questions'], b['questions'])

    def test_different_seed_differs(self):
        a = self.run_gen(7, 'a.json')
        b = self.run_gen(8, 'b.json')
        self.assertNotEqual([q['id'] for q in a['questions']],
                            [q['id'] for q in b['questions']])

    def test_symbol_name_absent_from_query(self):
        corpus = self.run_gen(7)
        names = ['fetch_widget', 'fetchWidget', 'decorate', 'WidgetStore',
                 'widget_store', 'widgetStore', 'rolling_checksum', 'rollingChecksum',
                 'renderAccountTable', 'render_account_table', 'harvest_field', 'harvestField']
        for q in corpus['questions']:
            for name in names:
                for match in __import__('re').findall(r'\b[A-Za-z_][A-Za-z0-9_]*\b', q['query']):
                    self.assertNotEqual(match, name, q['query'])

    def test_gold_snippets_match_bytes(self):
        corpus = self.run_gen(7)
        roots = {'alpha': self.alpha, 'beta': self.beta}
        for q in corpus['questions']:
            for span in q['expected_spans']:
                raw = (roots[q['repository']] / span['path']).read_bytes()
                lines = raw.decode('utf-8').split('\n')
                text = '\n'.join(lines[span['start_line'] - 1:span['end_line']])
                if span['end_line'] < len(lines) or raw.endswith(b'\n'):
                    text += '\n'
                self.assertEqual(span['snippet'], text)
                self.assertEqual(span['sha256'],
                                 hashlib.sha256(text.encode('utf-8')).hexdigest())

    def test_negatives_absent_from_target(self):
        corpus = self.run_gen(7)
        roots = {r['id']: self.base / r['root'] for r in corpus['repositories']}
        target_idents = {}
        for r in corpus['repositories']:
            names = set()
            for rel in r['source_files']:
                names.update(gq.IDENT.findall((roots[r['id']] / rel).read_text()))
            target_idents[r['id']] = names
        for q in corpus['questions']:
            if 'negative' in q['tags']:
                self.assertEqual(q['expected_spans'], [])
                # The fixture's only beta symbol must be filtered if it were
                # present; here just check no beta-named symbol survives.
                for match in gq.IDENT.findall(q['query']):
                    self.assertNotIn(match, ('harvest_field', 'harvestField'))
            else:
                self.assertTrue(q['expected_spans'])

    def test_load_corpus_semantics(self):
        corpus = self.run_gen(7)
        self.assertEqual(corpus['schema_version'], 1)
        repos = {r['id']: r for r in corpus['repositories']}
        ids = set()
        for q in corpus['questions']:
            self.assertNotIn(q['id'], ids)
            ids.add(q['id'])
            self.assertEqual(q['split'], 'generated')
            repo = repos[q['repository']]
            root = (self.base / repo['root']).resolve()
            self.assertTrue(str(root).startswith(str(self.base.resolve())))
            self.assertNotEqual(bool(q['expected_spans']), 'negative' in q['tags'])
            for label in q['expected_spans']:
                self.assertNotIn('start_byte', label)
                self.assertIn(label['path'], repo['source_files'])


if __name__ == '__main__':
    unittest.main()
