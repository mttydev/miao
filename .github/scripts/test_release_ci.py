import importlib.util
from pathlib import Path
import re
import unittest

spec = importlib.util.spec_from_file_location("release_ci", Path(__file__).with_name("check-release-ci.py"))
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


def run(**fields):
    return {"id": 1, "head_sha": "release-sha", "head_branch": "main", "event": "push", "status": "completed", "conclusion": "success", "html_url": "https://example.invalid/run/1", **fields}


class ReleaseCITest(unittest.TestCase):
    def test_builds_and_publication_share_the_verified_commit(self):
        workflow = Path(__file__).resolve().parents[1].joinpath("workflows/release.yml").read_text()
        self.assertIn("ref: ${{ inputs.commit || github.sha }}", workflow)
        self.assertIn("commit: ${{ steps.commit.outputs.sha }}", workflow)
        # Every job that builds or publishes checks out the verified commit; only
        # the `version` job reads the dispatch commit, because it is the job that
        # resolves it. Counting pairs this way keeps the invariant true when a
        # job is added, instead of hard-coding how many jobs exist today.
        checkouts = workflow.count("uses: actions/checkout")
        verified = workflow.count("ref: ${{ needs.version.outputs.commit }}")
        self.assertEqual(checkouts, verified + 1)
        self.assertGreaterEqual(verified, 2)
        self.assertNotIn("ref: main", workflow)

    def test_tag_target_uses_the_verified_checkout(self):
        version = Path(__file__).resolve().parents[2].joinpath("script/version.ts").read_text()
        self.assertIn("git rev-parse HEAD", version)
        self.assertNotIn("process.env.GITHUB_SHA", version)

    def test_exact_main_push_passes(self):
        self.assertEqual(release.require_success([run()], "release-sha", "test.yml"), "https://example.invalid/run/1")

    def test_missing_wrong_sha_branch_or_event_fails(self):
        for runs in [[], [run(head_sha="other")], [run(head_branch="feature")], [run(event="pull_request")]]:
            with self.assertRaisesRegex(ValueError, "no main push run"):
                release.require_success(runs, "release-sha", "test.yml")

    def test_unfinished_or_failed_run_cannot_publish(self):
        for status, conclusion in [("queued", None), ("in_progress", None), ("completed", "failure"), ("completed", "cancelled"), ("completed", "skipped")]:
            with self.assertRaises(ValueError):
                release.require_success([run(status=status, conclusion=conclusion)], "release-sha", "test.yml")

    def test_new_run_supersedes_an_older_success(self):
        with self.assertRaises(ValueError):
            release.require_success([run(), run(id=2, status="in_progress", conclusion=None)], "release-sha", "test.yml")
        self.assertEqual(release.require_success([run(conclusion="failure"), run(id=2)], "release-sha", "test.yml"), "https://example.invalid/run/1")

    def test_gate_outlasts_one_wedged_job_and_its_automatic_retry(self):
        # A job wedged on a self-hosted runner is killed only when its own
        # `timeout-minutes` runs out, and retry-cancelled then re-runs it for
        # another full job budget. A gate shorter than both aborted a release
        # on 2026-11-18 while its own automation was still recovering, so this
        # test derives the budget from the workflows instead of restating it.
        workflows = Path(__file__).resolve().parents[1].joinpath("workflows")
        ci = workflows.joinpath("ci.yml").read_text()
        guarded = [workflows.joinpath("ci.yml")] + [
            workflows.joinpath(Path(used).name)
            for used in re.findall(r"uses: \./\.github/workflows/([\w-]+\.yml)", ci)
        ]
        longest_job = max(
            int(match.group(1))
            for path in guarded
            for match in re.finditer(r"timeout-minutes:\s*(\d+)", path.read_text())
        )
        self.assertGreaterEqual(release.WAIT_SECONDS, 2 * longest_job * 60 + 10 * 60)

        release_yml = Path(__file__).resolve().parents[1].joinpath("workflows/release.yml").read_text()
        version_job = release_yml.split("\n  cli:")[0]
        version_timeout = int(re.search(r"timeout-minutes:\s*(\d+)", version_job).group(1))
        self.assertGreaterEqual(version_timeout, release.WAIT_SECONDS // 60 + 15)


if __name__ == "__main__":
    unittest.main()
