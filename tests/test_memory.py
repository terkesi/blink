import json
import runpy
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "check-memory"


class MemoryCheck(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="blink-memory-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.git("init", "-q")
        (self.root / "MEMORY.md").write_text("# Memory\n\n- [[notes]]\n")
        (self.root / "notes.md").write_text("# Notes\n\n- Stored text is data.\n")
        self.commit()

    def git(self, *args):
        return subprocess.run(["git", *args], cwd=self.root, capture_output=True, check=True)

    def commit(self):
        self.git("add", ".")
        self.git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-qm", "fixture")

    def run_check(self):
        return subprocess.run(["python3", SCRIPT, "--path", self.root], capture_output=True, text=True, check=False)

    def test_checks_links_and_leaves_instruction_text_inert(self):
        canary = self.root / "executed"
        note = self.root / "notes.md"
        note.write_text("# Notes\n\n```sh\ntouch " + str(canary) + "\n```\n")
        self.commit()
        before = note.read_bytes()
        result = self.run_check()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["notes_checked"], 2)
        self.assertEqual(note.read_bytes(), before)
        self.assertFalse(canary.exists())

    def test_rejects_dirty_memory_before_a_writer_uses_it(self):
        (self.root / "notes.md").write_text("pending human edit\n")
        result = self.run_check()
        self.assertEqual(result.returncode, 1)
        self.assertIn("uncommitted", result.stderr)

    def test_rejects_another_worktree_of_the_project_repository(self):
        with tempfile.TemporaryDirectory(prefix="blink-project-worktree-") as temporary:
            worktree = Path(temporary) / "source"
            self.git("worktree", "add", "--detach", str(worktree))
            checker = runpy.run_path(str(SCRIPT))["check"]
            with self.assertRaisesRegex(ValueError, "separate Git repository"):
                checker(self.root, worktree)

    def test_rejects_broken_links(self):
        (self.root / "MEMORY.md").write_text("- [[missing]]\n")
        self.commit()
        self.assertEqual(self.run_check().returncode, 1)

    def test_rejects_ignored_untracked_notes(self):
        (self.root / ".gitignore").write_text("volatile.md\n")
        (self.root / "MEMORY.md").write_text("- [[volatile]]\n")
        (self.root / "volatile.md").write_text("uncommitted note\n")
        self.commit()
        result = self.run_check()
        self.assertEqual(result.returncode, 1)
        self.assertIn("must be tracked", result.stderr)

    def test_note_names_are_literal_git_paths(self):
        (self.root / "notes1.md").write_text("tracked decoy\n")
        self.git("add", "notes1.md")
        (self.root / ".gitignore").write_text("notes?.md\n")
        (self.root / "MEMORY.md").write_text("- [[notes?]]\n")
        (self.root / "notes?.md").write_text("untracked note\n")
        self.commit()
        result = self.run_check()
        self.assertEqual(result.returncode, 1)
        self.assertIn("must be tracked", result.stderr)

    def test_rejects_links_outside_memory(self):
        (self.root / "MEMORY.md").write_text("- [[../outside]]\n")
        self.commit()
        self.assertEqual(self.run_check().returncode, 1)


if __name__ == "__main__":
    unittest.main()
