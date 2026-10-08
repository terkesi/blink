import importlib.util, io, json, os, shutil, tempfile, unittest
from contextlib import redirect_stdout
from pathlib import Path

HERE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_loader('skill_follow', importlib.machinery.SourceFileLoader('skill_follow', str(HERE.parent / 'scripts/skill-follow')))
sf = importlib.util.module_from_spec(SPEC); SPEC.loader.exec_module(sf)


@unittest.skipIf(shutil.which('rg') is None, 'ripgrep not installed')
class SkillFollowTest(unittest.TestCase):
    def setUp(self):
        self.dir = Path(tempfile.mkdtemp())
        root = self.dir / 'repo'; root.mkdir()
        (root / 'a.py').write_text('def caller():\n    return helper_value(3)\n')
        (root / 'b.py').write_text('# b\n\ndef helper_value(x):\n    y = x\n    return y * 2\n')
        a = (root / 'a.py').read_bytes()
        corpus = dict(schema_version=1, repositories=[dict(id='r', root='repo', source_files=['a.py', 'b.py'], excluded_files=[], split='generated')],
                      questions=[dict(id='q1', repository='r', split='generated', query='q', tags=['positive'],
                                      expected_spans=[dict(path='a.py', start_line=1, end_line=2, snippet='', sha256=''), dict(path='b.py', start_line=4, end_line=5, snippet='', sha256='')]),
                                 dict(id='n1', repository='r', split='generated', query='q', tags=['negative'], expected_spans=[])])
        (self.dir / 'corpus.json').write_text(json.dumps(corpus))
        run = self.dir / 'run'; (run / '01-blink').mkdir(parents=True); (run / '02-blink').mkdir()
        (run / '01-blink/trial.json').write_text(json.dumps(dict(id='q1', tool='blink', metrics={}, parsed=dict(results=[dict(path='a.py', start_line=1, end_line=2, start_byte=0, end_byte=len(a))]))))
        (run / '02-blink/trial.json').write_text(json.dumps(dict(id='n1', tool='blink', metrics={}, parsed=dict(results=[]))))

    def tearDown(self):
        shutil.rmtree(self.dir)

    def test_hop_reaches_the_callee_body_and_leaves_negatives_alone(self):
        buf = io.StringIO()
        with redirect_stdout(buf):
            sf.main(['--corpus', str(self.dir / 'corpus.json'), str(self.dir / 'run'), '--tools', 'blink'])
        report = json.loads(buf.getvalue())['blink']
        self.assertEqual((report['complete_alone'], report['complete_with_hop']), (0, 1))
        self.assertEqual((report['spans_alone'], report['spans_with_hop']), (1, 2))
        self.assertEqual((report['negative_source_alone'], report['negative_reached_with_hop']), (0, 0))
        self.assertGreaterEqual(report['searches'], 1)

    def test_names_prefer_called_helpers_and_skip_keywords(self):
        names, defined = sf.names_in('def caller():\n    if ready(x):\n        return helper_value(3)\n', 6)
        self.assertEqual(defined, {'caller'})
        self.assertEqual(names[:2], ['ready', 'helper_value'])


if __name__ == '__main__':
    unittest.main()
