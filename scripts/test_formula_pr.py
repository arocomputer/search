"""Protect formula PR ownership and idempotency without contacting GitHub."""

import json
import os
import unittest
from unittest.mock import patch

from formula_pr import publish


class FormulaPRTests(unittest.TestCase):
    """Publication must never replace an existing branch or human edits."""

    def execute(self, existing=False, author="app/github-actions", state="OPEN",
                commit_author="github-actions[bot]", files="search.rb", content="formula",
                orphan=False, raced=False, changed=True):
        """Run the publisher against a small command fake and record mutations."""
        calls = []
        def command(*args, **kwargs):
            """Return fixture state for reads and record every command."""
            calls.append(args)
            if args[:4] == ("git", "diff", "--cached", "--name-only"):
                return "search.rb" if changed else ""
            if args[:3] == ("gh", "pr", "list"):
                return json.dumps([dict(number=1, author=dict(login=author), state=state,
                                        headRefOid="abc", headRepository=dict(name="search"), headRepositoryOwner=dict(login="owner"))]
                                  if existing and not orphan else [])
            if args[:2] == ("git", "ls-remote"):
                return "abc refs/heads/release/formula-v1.2.3" if existing else ""
            if args[:2] == ("gh", "api"):
                return json.dumps(dict(author=dict(login=commit_author), parents=[dict(sha="base")],
                                       commit=dict(message="release: update Homebrew formula for v1.2.3")))
            if args[:3] == ("git", "diff", "--name-only"):
                return files
            if args[:2] == ("git", "rev-parse"):
                if args[2] == "FETCH_HEAD":
                    return "abc"
                return "formula" if args[2] == ":search.rb" else content
            if "push" in args and raced:
                raise RuntimeError("Branch appeared concurrently")
            return ""
        with patch.dict(os.environ, GH_REPO="owner/search", RELEASE_TOKEN="test-placeholder"), patch("formula_pr.run", side_effect=command):
            publish("v1.2.3")
        return calls

    def test_no_change_creates_nothing(self):
        calls = self.execute(changed=False)
        self.assertFalse(any(c[0] == "gh" or "push" in c for c in calls))

    def test_new_update_creates_only_reserved_branch_and_pr(self):
        calls = self.execute()
        push = next(c for c in calls if "push" in c)
        self.assertIn("--force-with-lease=refs/heads/release/formula-v1.2.3:", push)
        self.assertEqual(push[-1], "HEAD:refs/heads/release/formula-v1.2.3")
        self.assertEqual(sum(c[:3] == ("gh", "pr", "create") for c in calls), 1)
        self.assertFalse(any("merge" in c for c in calls))

    def test_matching_bot_pr_is_reused_without_writes(self):
        calls = self.execute(existing=True)
        self.assertFalse(any("push" in c or c[:3] == ("gh", "pr", "create") for c in calls))

    def test_pr_creation_failure_can_be_retried_without_pushing_again(self):
        calls = self.execute(existing=True, orphan=True)
        self.assertFalse(any("push" in c for c in calls))
        self.assertEqual(sum(c[:3] == ("gh", "pr", "create") for c in calls), 1)

    def test_foreign_or_modified_branch_is_refused(self):
        for change in (dict(author="human"), dict(commit_author="human"),
                       dict(files="search.rb\nREADME.md"), dict(content="human edit"),
                       dict(state="CLOSED")):
            with self.subTest(change=change), self.assertRaises(ValueError):
                self.execute(existing=True, **change)

    def test_branch_creation_race_aborts_before_pr(self):
        with self.assertRaises(RuntimeError):
            self.execute(raced=True)


class ReleaseEnvironmentTests(unittest.TestCase):
    """Every release-token consumer must use the protected release environment."""

    def test_token_jobs_declare_release_environment(self):
        from pathlib import Path
        import re
        workflows = Path(__file__).resolve().parents[1] / ".github/workflows"
        for workflow in workflows.glob("*.yml"):
            for job in re.split(r"(?m)^  [A-Za-z_][A-Za-z_0-9-]*:\n", workflow.read_text())[1:]:
                if "secrets.RELEASE_TOKEN" in job:
                    with self.subTest(workflow=workflow.name):
                        self.assertRegex(job, r"(?m)^    environment: release$")
