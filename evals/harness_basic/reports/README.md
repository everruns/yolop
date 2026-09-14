# Committed eval reports

This directory retains the complete JSON report for every harness run used as
durable evidence in a pull request, knowledge entry, release decision, or other
repository conclusion. Keep the original Mira run ID as the directory name.

Copy `results/<run_id>/report.json` here after the run, review it for credentials
and host-sensitive values, and commit it with the change that cites the result.
Raw HTML, machine metadata, case workspaces, and exploratory runs stay in the
gitignored `results/` archive.
