#!/usr/bin/env python3
"""Offline formula publication contracts using real Git refs and a fake GitHub CLI."""
import json
import os
import shutil
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class FormulaPR(unittest.TestCase):
    """Protect retry behavior and refusal to replace human-owned work."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.remote = self.root / "remote.git"
        self.work = self.root / "work"
        self.run_cmd("git", "init", "--bare", str(self.remote), cwd=self.root)
        self.run_cmd("git", "init", "-b", "main", str(self.work), cwd=self.root)
        self.git("config", "user.name", "Maintainer")
        self.git("config", "user.email", "maintainer@example.invalid")
        (self.work / "search.rb").write_text("old formula\n")
        self.git("add", ".")
        self.git("commit", "-m", "initial")
        self.git("remote", "add", "origin", str(self.remote))
        self.git("push", "-u", "origin", "main")
        self.base = self.git("rev-parse", "HEAD").stdout.strip()
        self.state = self.root / "prs.json"
        self.state.write_text("[]")
        self.log = self.root / "calls"
        fake = self.root / "gh"
        fake.write_text(r"""#!/usr/bin/env python3
import json, os, pathlib, subprocess, sys
args = sys.argv[1:]
state = pathlib.Path(os.environ['PR_STATE'])
with open(os.environ['PR_LOG'], 'a') as log:
    log.write(' '.join(args) + '\n')
if args[:2] == ['pr', 'list']:
    print(state.read_text())
elif args[:2] == ['pr', 'create']:
    head = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
    state.write_text(json.dumps([dict(number=1, state='OPEN', author=dict(login='github-actions[bot]'), headRefOid=head, isCrossRepository=False)]))
""")
        fake.chmod(0o755)
        self.env = dict(os.environ, PATH=str(self.root) + os.pathsep + os.environ["PATH"],
                        PR_STATE=str(self.state), PR_LOG=str(self.log))

    def run_cmd(self, *args, cwd=None, check=True, env=None):
        """Run fixture commands without exposing inherited credentials in output."""
        return subprocess.run(args, cwd=cwd or self.work, env=env,
                              text=True, capture_output=True, check=check)

    def git(self, *args):
        """Run Git in the disposable checkout."""
        return self.run_cmd("git", *args)

    def publish(self, change=True):
        """Regenerate from main exactly as the workflow does on every attempt."""
        self.git("reset", "--hard", self.base)
        if change:
            (self.work / "search.rb").write_text("new formula\n")
        return self.run_cmd("bash", str(ROOT / "scripts/formula-pr.bash"),
                            "v1.2.3", "search.rb", env=self.env, check=False)

    def branch_head(self):
        """Read the reserved remote ref without changing it."""
        return self.git("ls-remote", "origin", "refs/heads/release/formula-v1.2.3").stdout

    def test_no_change_creates_neither_branch_nor_pr(self):
        self.assertEqual(self.publish(change=False).returncode, 0)
        self.assertEqual(self.branch_head(), "")
        self.assertFalse(self.log.exists())

    def test_publish_and_retry_keep_one_pr_and_same_head(self):
        self.assertEqual(self.publish().returncode, 0)
        head = self.branch_head()
        self.assertEqual(self.publish().returncode, 0)
        self.assertEqual(self.branch_head(), head)
        calls = self.log.read_text()
        self.assertEqual(calls.count("pr create"), 1)
        self.assertEqual(calls.count("workflow run ci.yml"), 2)
        self.assertEqual(self.git("ls-remote", "origin", "refs/heads/main").stdout.split()[0], self.base)

    def test_retry_recovers_push_before_pr_creation(self):
        self.assertEqual(self.publish().returncode, 0)
        self.state.write_text("[]")
        head = self.branch_head()
        self.assertEqual(self.publish().returncode, 0)
        self.assertEqual(self.branch_head(), head)
        self.assertEqual(len(json.loads(self.state.read_text())), 1)

    def test_human_commit_is_preserved_even_with_bot_message(self):
        self.assertEqual(self.publish().returncode, 0)
        self.git("config", "user.name", "Maintainer")
        self.git("config", "user.email", "maintainer@example.invalid")
        (self.work / "notes").write_text("human edit")
        self.git("add", "notes")
        self.git("commit", "-m", "chore: update formula for v1.2.3")
        self.git("push", "origin", "HEAD:refs/heads/release/formula-v1.2.3")
        head = self.branch_head()
        self.assertNotEqual(self.publish().returncode, 0)
        self.assertEqual(self.branch_head(), head)

    def test_concurrent_branch_creation_is_not_overwritten(self):
        real_git = shutil.which("git")
        wrapper = self.root / "git"
        wrapper.write_text('#!/bin/sh\ncase "$*" in\n  *HEAD:refs/heads/release/formula-v1.2.3*)\n    "$REAL_GIT" --git-dir="$REMOTE" update-ref refs/heads/release/formula-v1.2.3 "$BASE" ;;\nesac\nexec "$REAL_GIT" "$@"\n')
        wrapper.chmod(0o755)
        self.env.update(REAL_GIT=real_git, REMOTE=str(self.remote), BASE=self.base)
        self.assertNotEqual(self.publish().returncode, 0)
        self.assertEqual(self.branch_head().split()[0], self.base)
        self.assertEqual(self.state.read_text(), "[]")

    def test_human_branch_without_pr_is_preserved(self):
        (self.work / "search.rb").write_text("new formula\n")
        self.git("add", "search.rb")
        self.git("commit", "-m", "chore: update formula for v1.2.3")
        self.git("push", "origin", "HEAD:refs/heads/release/formula-v1.2.3")
        head = self.branch_head()
        self.assertNotEqual(self.publish().returncode, 0)
        self.assertEqual(self.branch_head(), head)
        self.assertEqual(self.state.read_text(), "[]")

    def test_modified_formula_and_file_mode_are_refused(self):
        for mode in (False, True):
            with self.subTest(mode=mode):
                # Replace fixture branch with a single bot-authored unexpected change.
                self.git("reset", "--hard", self.base)
                (self.work / "search.rb").write_text("new formula\n" if mode else "human formula\n")
                self.git("config", "user.name", "github-actions[bot]")
                self.git("config", "user.email", "41898282+github-actions[bot]@users.noreply.github.com")
                self.git("add", "search.rb")
                if mode:
                    self.git("update-index", "--chmod=+x", "search.rb")
                self.git("commit", "-m", "chore: update formula for v1.2.3")
                self.git("push", "--force", "origin", "HEAD:refs/heads/release/formula-v1.2.3")
                head = self.branch_head()
                self.assertNotEqual(self.publish().returncode, 0)
                self.assertEqual(self.branch_head(), head)

    def test_foreign_or_closed_pr_is_not_modified(self):
        self.assertEqual(self.publish().returncode, 0)
        original = json.loads(self.state.read_text())
        for field, value in (("author", dict(login="maintainer")), ("state", "CLOSED"),
                             ("isCrossRepository", True), ("headRefOid", self.base)):
            with self.subTest(field=field):
                pr = dict(original[0], **{field: value})
                self.state.write_text(json.dumps([pr]))
                head = self.branch_head()
                self.assertNotEqual(self.publish().returncode, 0)
                self.assertEqual(self.branch_head(), head)
                self.assertEqual(json.loads(self.state.read_text()), [pr])

    def test_first_formula_creation_is_published(self):
        self.git("rm", "search.rb")
        self.git("commit", "-m", "before first formula")
        self.git("push", "origin", "main")
        self.base = self.git("rev-parse", "HEAD").stdout.strip()
        self.assertEqual(self.publish().returncode, 0)
        self.assertNotEqual(self.branch_head(), "")


class WorkflowContract(unittest.TestCase):
    """Every job that consumes the release credential must use its environment."""

    def test_release_token_jobs_use_release_environment(self):
        for workflow in (ROOT / ".github/workflows").glob("*.yml"):
            job = []
            for line in workflow.read_text().splitlines() + ["  end:"]:
                if line.startswith("  ") and not line.startswith("   ") and line.endswith(":"):
                    if any("secrets.RELEASE_TOKEN" in entry for entry in job):
                        self.assertIn("    environment: release", job, str(workflow))
                    job = []
                job.append(line)


if __name__ == "__main__":
    unittest.main()
