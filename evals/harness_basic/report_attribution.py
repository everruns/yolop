#!/usr/bin/env python3
"""Index eval spend and existing provider session metadata without another API call."""
import argparse
import json
import os
import re
from pathlib import Path

SESSION_SUFFIX = re.compile(r"-([0-9a-f]{32})\.events\.jsonl$")
RUN_FIELDS = {
    "repository": "GITHUB_REPOSITORY", "workflow": "GITHUB_WORKFLOW",
    "run_id": "GITHUB_RUN_ID", "run_attempt": "GITHUB_RUN_ATTEMPT",
    "commit": "GITHUB_SHA", "event": "GITHUB_EVENT_NAME",
    "actor": "GITHUB_ACTOR", "triggering_actor": "GITHUB_TRIGGERING_ACTOR",
}


def build_manifest(reports, env):
    run = {key: env.get(value) for key, value in RUN_FIELDS.items()}
    server = env.get("GITHUB_SERVER_URL", "https://github.com")
    run["url"] = (f"{server}/{run['repository']}/actions/runs/{run['run_id']}"
                  if run["repository"] and run["run_id"] else None)
    cases = []
    sources = []
    for name, report in reports:
        sources.append({"report": name, "reported_cost_usd": report.get("summary", {}).get("total_cost_usd")})
        for index, case in enumerate(report.get("cases", [])):
            transcript = case.get("transcript") or {}
            metadata = transcript.get("metadata") or {}
            match = SESSION_SUFFIX.search(metadata.get("session_log", ""))
            usage = transcript.get("usage") or {}
            cases.append({
                "report": name, "eval": case.get("eval"), "sample": case.get("sample"),
                "case_index": index, "trials": case.get("trials"), "params": case.get("params", {}),
                "target": case.get("target"), "passed": case.get("passed"),
                "session_id": f"session_{match[1]}" if match else None,
                "usage": {key: usage.get(key) for key in (
                    "input_tokens", "output_tokens", "cache_read_tokens", "cache_creation_tokens", "cost_usd")},
            })
    return {"schema_version": 1, "run": run, "reports": sources, "cases": cases,
            "reported_cost_usd": sum(c["usage"]["cost_usd"] or 0 for c in cases),
            "unattributed_cases": sum(c["session_id"] is None for c in cases),
            "cases_without_cost": sum(c["usage"]["cost_usd"] is None for c in cases),
            "cost_basis": "Harness-reported costs: provider actual cost when available, otherwise price-table estimates. Only output.message.completed usage is mined; auxiliary calls such as completion review may be excluded. Not an invoice reconciliation; missing usage is unknown, not free."}


def summary(manifest):
    return ("## Evaluation cost attribution\n\n"
            f"- Reported cost: ${manifest['reported_cost_usd']:.6f}\n"
            f"- Cases: {len(manifest['cases'])}; without a session join: {manifest['unattributed_cases']}; without cost: {manifest['cases_without_cost']}\n"
            "- Artifact: `eval-attribution.json`. Match its `session_id` to provider request metadata.\n"
            "- Costs are harness-reported, not billing reconciliation. Auxiliary calls such as completion review may be excluded. Missing usage is unknown, not free.\n"
            "- GitHub actor identifies credentials used, not whether a human or an agent initiated the run.\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("reports", nargs="+", type=Path)
    parser.add_argument("--out", required=True, type=Path)
    args = parser.parse_args()
    reports, missing, invalid = [], [], []
    for path in args.reports:
        try:
            report = json.loads(path.read_text())
        except FileNotFoundError:
            missing.append(path.name)
        except (OSError, UnicodeError, json.JSONDecodeError) as error:
            invalid.append({"report": path.name, "error": type(error).__name__})
        else:
            reports.append((path.name, report))
    manifest = build_manifest(reports, os.environ)
    manifest["missing_reports"] = missing
    manifest["invalid_reports"] = invalid
    args.out.write_text(json.dumps(manifest, indent=2) + "\n")
    text = summary(manifest)
    if manifest["missing_reports"]:
        text += "- Missing reports: " + ", ".join(manifest["missing_reports"]) + "\n"
    if invalid:
        text += "- Invalid reports: " + ", ".join(item["report"] for item in invalid) + "\n"
    print(text)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as stream:
            stream.write(text)


if __name__ == "__main__":
    main()
