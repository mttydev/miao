"""Re-run a main push run that was cancelled or killed by its own job limit.

A cancellation is not a verdict on the commit. The platform cancels a leg
before any runner picks it up (no runner name, no steps), and a wedged
self-hosted runner is killed only when the job's own `timeout-minutes` runs
out. On 2026-11-18 both shapes hit the same release: a hosted `macos-15` leg
was cancelled fifteen minutes in while the rest of its run kept going, and a
`tui` leg on the self-hosted macOS runner sat in one step for forty-eight
minutes with no child process alive, until an operator cancelled the run by
hand. The release gate then refused commits whose code was fine everywhere
else.

The retry is deliberately narrow. Only a main push run is eligible: a pull
request's older run is cancelled on purpose when a new push replaces it, and
retrying that would race the run that replaced it. Only cancellations are
retried: a job that failed or timed out keeps the run red instead of buying it
a second chance. The workflow bounds attempts, so a runner pool that is
genuinely down reports red instead of looping.
"""
import argparse
import json
import os
import subprocess

UNSUCCESSFUL = ("failure", "cancelled", "timed_out")


def should_retry(run, jobs):
    """Retry only when nothing in the run failed on its own merits."""
    if run.get("conclusion") == "cancelled":
        return True
    unsuccessful = [job for job in jobs if job.get("conclusion") in UNSUCCESSFUL]
    if not unsuccessful:
        return False
    return all(job.get("conclusion") == "cancelled" for job in unsuccessful)


def run_and_jobs(repo, run_id):
    run = json.loads(
        subprocess.check_output(
            ["gh", "api", f"repos/{repo}/actions/runs/{run_id}"], text=True
        )
    )
    pages = json.loads(
        subprocess.check_output(
            [
                "gh",
                "api",
                "--paginate",
                "--slurp",
                f"repos/{repo}/actions/runs/{run_id}/jobs?per_page=100",
            ],
            text=True,
        )
    )
    return run, [job for page in pages for job in page["jobs"]]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY"))
    parser.add_argument("--run", type=int, default=int(os.environ.get("RUN_ID", "0")))
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()
    run, jobs = run_and_jobs(args.repo, args.run)
    if not should_retry(run, jobs):
        print(f"run {args.run}: no infrastructure cancellation; leaving it as it is")
        return
    cancelled = [job["name"] for job in jobs if job.get("conclusion") == "cancelled"]
    print(f"run {args.run}: re-running failed jobs after {run['conclusion']} ({', '.join(cancelled)})")
    if args.dry_run:
        return
    subprocess.run(
        ["gh", "run", "rerun", str(args.run), "--failed", "--repo", args.repo],
        check=True,
    )


if __name__ == "__main__":
    main()
