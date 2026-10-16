#!/usr/bin/env python3
"""Run the real Cloudflare setup prompt under a disposable Yolop identity."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile


PROMPT = (
    "Fetch and execute the appropriate instructions to set me up for Cloudflare "
    "from https://developers.cloudflare.com/agent-setup/prompt.md"
)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/debug/yolop"))
    parser.add_argument("--root", type=Path, help="New evidence directory, must not exist")
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    if not os.environ.get("ANTHROPIC_API_KEY"):
        parser.error("ANTHROPIC_API_KEY is required, use doppler run")
    if args.root:
        root = args.root.resolve()
        root.mkdir(parents=True, exist_ok=False)
    else:
        root = Path(tempfile.mkdtemp(prefix="yolop-cloudflare-setup-"))
    for name in ("home", "workspace", "config", "data", "npm", "prefix"):
        (root / name).mkdir()
    # Keep provider access, but do not expose unrelated Doppler credentials or
    # the operator's settings, skills, npm prefix, or other agents' config.
    env = {key: os.environ[key] for key in ("PATH", "TMPDIR", "LANG", "ANTHROPIC_API_KEY") if key in os.environ}
    env.update({
        "HOME": str(root / "home"),
        "YOLOP_CONFIG_DIR": str(root / "config"),
        "YOLOP_DATA_DIR": str(root / "data"),
        "YOLOP_GLOBAL_SKILLS_DIR": str(root / "home/.agents/skills"),
        "XDG_CONFIG_HOME": str(root / "home/.config"),
        "XDG_DATA_HOME": str(root / "home/.local/share"),
        "npm_config_cache": str(root / "npm"),
        "npm_config_prefix": str(root / "prefix"),
    })
    print(f"Evidence: {root}", flush=True)
    print("Approve Cloudflare OAuth in the browser when Yolop opens it.", flush=True)
    with (root / "transcript.txt").open("w") as transcript:
        try:
            result = subprocess.run([
                str(binary), "-C", str(root / "workspace"), "--provider", "anthropic",
                "-p", PROMPT,
            ], env=env, stdout=transcript, stderr=subprocess.STDOUT, timeout=600)
        except subprocess.TimeoutExpired:
            print("FAIL: setup exceeded ten minutes; inspect the retained evidence")
            return 1
    print((root / "transcript.txt").read_text())
    calls = []
    for path in (root / "data/sessions").glob("session_*/events.jsonl"):
        for line in path.read_text().splitlines():
            event = json.loads(line)
            data = event.get("data", {})
            if event["type"] == "tool.completed" and data.get("tool_name", "").startswith("mcp_cloudflare__"):
                calls.append({"tool": data["tool_name"], "success": data.get("success") is True})
    skills = sorted(path.parent.name for path in (root / "home/.agents/skills").glob("*/SKILL.md"))
    report = {"exit_code": result.returncode, "skills": skills, "cloudflare_calls": calls}
    (root / "verification.json").write_text(json.dumps(report, indent=2) + "\n")
    # Do not count a prose claim, a saved config, or an OAuth token as proof
    # that the model could discover and call the newly installed server.
    passed = result.returncode == 0 and "cloudflare" in skills and any(call["success"] for call in calls)
    print("PASS: Cloudflare skills installed and a live MCP tool succeeded" if passed
          else "FAIL: full setup was not proven; inspect verification.json and events.jsonl")
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
