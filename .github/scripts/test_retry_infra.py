import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location(
    "retry_infra", Path(__file__).with_name("retry-infrastructure-cancellation.py")
)
retry = importlib.util.module_from_spec(spec)
spec.loader.exec_module(retry)


def job(name="leg", conclusion="success", runner="ubuntu-24.04", steps=8):
    return {"name": name, "conclusion": conclusion, "runner_name": runner, "steps": [None] * steps}


def run(conclusion="failure"):
    return {"conclusion": conclusion}


class RetryInfrastructureCancellationTest(unittest.TestCase):
    def test_a_leg_the_platform_cancelled_before_a_runner_picked_it_up_is_retried(self):
        jobs = [job("ok"), job("native", conclusion="cancelled", runner="", steps=0), job("skipped", conclusion="skipped")]
        self.assertTrue(retry.should_retry(run(), jobs))

    def test_a_wedged_leg_killed_by_its_job_limit_is_retried(self):
        jobs = [job("ok"), job("tui", conclusion="cancelled", runner="macbook-engine-1", steps=4)]
        self.assertTrue(retry.should_retry(run(), jobs))

    def test_a_real_failure_keeps_the_run_red(self):
        for failed in ("failure", "timed_out"):
            jobs = [job("ok"), job("bad", conclusion=failed), job("native", conclusion="cancelled", runner="", steps=0)]
            self.assertFalse(retry.should_retry(run(), jobs), failed)

    def test_a_whole_run_cancelled_is_retried(self):
        self.assertTrue(retry.should_retry(run("cancelled"), [job("ok")]))

    def test_a_green_run_is_left_alone(self):
        self.assertFalse(retry.should_retry(run("success"), [job("ok"), job("skipped", conclusion="skipped")]))

    def test_one_healthy_leg_and_one_cancelled_leg_still_retries(self):
        jobs = [job("ok"), job("cancelled-late", conclusion="cancelled")]
        self.assertTrue(retry.should_retry(run(), jobs))


if __name__ == "__main__":
    unittest.main()
