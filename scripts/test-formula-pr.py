#!/usr/bin/env python3
"""Offline publication contracts using real Git refs and a stubbed GitHub CLI."""
import json
import os
import re
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("formula-pr.sh").resolve()


class FormulaPR(unittest.TestCase):
    """Exercise no-op, retry, and ownership behavior without credentials or GitHub."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.remote = self.root / "remote.git"
        self.git("init", "--bare", str(self.remote), cwd=self.root)
        self.work = self.root / "work"
        self.git("clone", str(self.remote), str(self.work), cwd=self.root)
        self.git("checkout", "-b", "main")
        self.git("config", "user.name", "fixture")
        self.git("config", "user.email", "fixture@example.invalid")
        (self.work / "tap.rb").write_text("old\n")
        (self.work / "scripts").mkdir()
        (self.work / "scripts/formula.sh").write_text("#!/bin/sh\nprintf 'new\\n'\n")
        (self.work / "scripts/formula.sh").chmod(0o755)
        self.git("add", ".")
        self.git("commit", "-m", "initial")
        self.git("push", "origin", "main")
        self.git("fetch", "origin")
        self.bin = self.root / "bin"
        self.bin.mkdir()
        gh = self.bin / "gh"
        gh.write_text(r"""#!/usr/bin/env python3
import json, os, subprocess, sys
from pathlib import Path
args = sys.argv[1:]
with open(os.environ["EVENTS"], "a") as f:
    f.write(json.dumps(args) + "\n")
if args[:2] == ["release", "download"]:
    Path(args[args.index("-O") + 1]).write_text("checksums")
elif args[:2] == ["pr", "list"]:
    prs = json.loads(os.environ.get("PRS", "[]"))
    owned = all(p["author"]["login"] in ("github-actions[bot]", "app/github-actions")
                and not p["isCrossRepository"] for p in prs)
    print(str(len(prs)) + " " + str(owned).lower())
elif args[:2] == ["auth", "setup-git"] and os.environ.get("RACE_HEAD"):
    subprocess.run(["git", "push", "origin", os.environ["RACE_HEAD"] +
                    ":refs/heads/release/formula-v1.2.3"], check=True, capture_output=True)
elif args[0] == "api":
    print(os.environ.get("AUTHOR", "github-actions[bot]"))
elif args[:2] == ["pr", "create"] and os.environ.get("FAIL_CREATE"):
    sys.exit(1)
""")
        gh.chmod(0o755)
        self.events = self.root / "events"
        self.env = dict(os.environ, PATH=str(self.bin) + os.pathsep + os.environ["PATH"],
                        EVENTS=str(self.events), RUNNER_TEMP=str(self.root),
                        GITHUB_REPOSITORY="fixture/repo")
        self.ref = "refs/heads/release/formula-v1.2.3"

    def git(self, *args, cwd=None):
        """Run Git in the disposable fixture and return its output."""
        return subprocess.check_output(["git", *args], cwd=cwd or self.work,
                                       stderr=subprocess.DEVNULL, text=True).strip()

    def run_script(self, success=True, reason=None):
        """Run the publisher and assert its success or intentional refusal."""
        result = subprocess.run(["sh", str(SCRIPT), "v1.2.3", "tap.rb"],
                                cwd=self.work, env=self.env, capture_output=True, text=True)
        self.assertEqual(result.returncode == 0, success, result.stderr)
        if reason:
            self.assertIn(reason, result.stderr)
        return [json.loads(line) for line in self.events.read_text().splitlines()] if self.events.exists() else []

    def reset(self):
        """Start a rerun from main without modifying the remote formula branch."""
        self.git("reset", "--hard", "origin/main")
        self.events.unlink(missing_ok=True)

    def bot_pr(self):
        """Describe an open PR owned by Actions in the same repository."""
        self.env["PRS"] = json.dumps([dict(number=1, author=dict(login="github-actions[bot]"),
                                          isCrossRepository=False)])

    def test_publishes_pr_without_touching_main(self):
        before = self.git("rev-parse", "origin/main")
        events = self.run_script()
        self.assertEqual(self.git("ls-remote", "origin", "refs/heads/main").split()[0], before)
        self.assertIn(["workflow", "run", "ci.yml", "--ref", "release/formula-v1.2.3"], events)
        self.assertEqual(sum(e[:2] == ["pr", "create"] for e in events), 1)

    def test_no_change_has_no_pr_or_branch(self):
        (self.work / "tap.rb").write_text("new\n")
        self.git("add", "tap.rb")
        self.git("commit", "-m", "already updated")
        events = self.run_script()
        self.assertFalse(any(e[:2] in (["pr", "create"], ["workflow", "run"]) for e in events))
        self.assertEqual(self.git("ls-remote", "origin", self.ref), "")

    def test_rerun_reuses_bot_pr_without_pushing(self):
        self.run_script()
        head = self.git("ls-remote", "origin", self.ref)
        self.reset()
        self.bot_pr()
        events = self.run_script()
        self.assertEqual(self.git("ls-remote", "origin", self.ref), head)
        self.assertFalse(any(e[:2] == ["pr", "create"] for e in events))

    def test_retry_recovers_after_pr_creation_failure(self):
        self.env["FAIL_CREATE"] = "1"
        self.run_script(False)
        head = self.git("ls-remote", "origin", self.ref)
        self.reset()
        del self.env["FAIL_CREATE"]
        events = self.run_script()
        self.assertEqual(self.git("ls-remote", "origin", self.ref), head)
        self.assertTrue(any(e[:2] == ["pr", "create"] for e in events))

    def test_human_pr_is_refused(self):
        self.env["PRS"] = '[{"number":1,"author":{"login":"human"},"isCrossRepository":false}]'
        self.run_script(False, "not owned by the bot")
        self.assertEqual(self.git("ls-remote", "origin", self.ref), "")

    def test_human_commit_is_refused_even_with_bot_metadata(self):
        self.run_script()
        self.reset()
        self.bot_pr()
        self.env["AUTHOR"] = "human"
        head = self.git("ls-remote", "origin", self.ref)
        self.run_script(False, "commit is not owned by the bot")
        self.assertEqual(self.git("ls-remote", "origin", self.ref), head)

    def test_additional_commit_is_preserved(self):
        self.run_script()
        (self.work / "tap.rb").write_text("human edit\n")
        self.git("add", ".")
        self.git("commit", "-m", "human edit")
        self.git("push", "origin", "HEAD:" + self.ref)
        head = self.git("ls-remote", "origin", self.ref)
        self.reset()
        self.bot_pr()
        self.run_script(False, "additional commits")
        self.assertEqual(self.git("ls-remote", "origin", self.ref), head)

    def test_different_formula_is_preserved(self):
        (self.work / "tap.rb").write_text("human edit\n")
        self.git("add", ".")
        self.git("commit", "-m", "release: formula v1.2.3")
        self.git("push", "origin", "HEAD:" + self.ref)
        head = self.git("ls-remote", "origin", self.ref)
        self.reset()
        self.bot_pr()
        self.run_script(False, "different content")
        self.assertEqual(self.git("ls-remote", "origin", self.ref), head)

    def test_branch_creation_race_preserves_human_commit(self):
        (self.work / "tap.rb").write_text("human edit\n")
        self.git("add", ".")
        self.git("commit", "-m", "human edit")
        human = self.git("rev-parse", "HEAD")
        self.env["RACE_HEAD"] = human
        self.reset()
        self.run_script(False, "rejected")
        self.assertEqual(self.git("ls-remote", "origin", self.ref).split()[0], human)

    def test_first_release_adds_untracked_formula(self):
        self.git("rm", "tap.rb")
        self.git("commit", "-m", "before first formula")
        events = self.run_script()
        self.assertTrue(any(e[:2] == ["pr", "create"] for e in events))



class ReleaseEnvironment(unittest.TestCase):
    """Keep every release-token consumer behind the release environment."""

    def test_token_jobs_declare_release_environment(self):
        workflow = SCRIPT.parent.parent / ".github/workflows/release.yml"
        jobs = re.split(r"(?m)^  [a-zA-Z_-]+:\n", workflow.read_text().split("jobs:\n", 1)[1])[1:]
        consumers = [job for job in jobs if "secrets.RELEASE_TOKEN" in job]
        self.assertTrue(consumers)
        for job in consumers:
            self.assertRegex(job, r"(?m)^    environment: release$")



if __name__ == "__main__":
    unittest.main()
