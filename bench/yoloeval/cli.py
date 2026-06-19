"""Command-line entry point for the evaluation harness.

Examples::

    # Smoke test: first instance, default config, run yolop but skip Docker eval
    python -m yoloeval run --limit 1 --no-eval

    # Real end-to-end on the first instance
    python -m yoloeval run --limit 1

    # A specific instance across the whole config matrix
    python -m yoloeval run --instance astropy__astropy-12907

    # Just (re)build summaries from saved results
    python -m yoloeval summarize --config openai-default
"""

from __future__ import annotations

import argparse
import json
import sys
from datetime import datetime, timezone

from .config import load_matrix
from .datasets.base import get_benchmark
from .runner import run_matrix
from .suites import suite_instance_ids
from . import results


def _select_instance_ids(benchmark, args) -> list[str] | None:
    if getattr(args, "suite", None):
        return suite_instance_ids(args.suite)
    if args.instance:
        return list(args.instance)
    if args.limit is not None:
        # Deterministic "first N" by dataset order.
        ids = [i.instance_id for i in benchmark.load()]
        return ids[: args.limit]
    return None


def cmd_run(args) -> int:
    defaults, configs = load_matrix(args.matrix)
    if args.max_cost is not None:
        # CLI override wins over the matrix default for every config.
        defaults["max_cost_usd"] = args.max_cost
        for cfg in configs.values():
            cfg.pop("max_cost_usd", None)
    if args.config:
        missing = [c for c in args.config if c not in configs]
        if missing:
            sys.exit(f"unknown config(s): {missing}; available: {list(configs)}")
        configs = {c: configs[c] for c in args.config}

    benchmark = get_benchmark(
        args.benchmark,
        max_workers=args.max_workers,
        namespace=args.namespace,
        timeout=args.eval_timeout,
    )
    instance_ids = _select_instance_ids(benchmark, args)

    overall = run_matrix(
        benchmark=benchmark,
        defaults=defaults,
        configs=configs,
        instance_ids=instance_ids,
        run_id=args.run_id,
        do_eval=not args.no_eval,
        keep_workdirs=args.keep_workdirs,
    )
    print("\n=== run complete ===")
    print(json.dumps(overall, indent=2))
    return 0


def cmd_eval(args) -> int:
    """Re-score already-saved patches without re-running the agent.

    Useful when the agent run succeeded but Docker evaluation failed for an
    infra reason (e.g. a Docker Hub pull rate limit), so we don't pay to
    regenerate patches just to score them.
    """
    benchmark = get_benchmark(
        args.benchmark, max_workers=args.max_workers,
        namespace=args.namespace, timeout=args.eval_timeout,
    )
    defaults, configs = load_matrix(args.matrix)
    names = args.config or list(configs)
    run_id = args.run_id or f"reeval-{datetime.now(timezone.utc):%Y%m%d-%H%M%S}"
    want = set(args.instance) if args.instance else None
    if getattr(args, "suite", None):
        want = set(suite_instance_ids(args.suite))
    limit_ids = None
    if args.limit is not None:
        limit_ids = {i.instance_id for i in benchmark.load()[: args.limit]}

    for name in names:
        cfg_dir = results.RESULTS_DIR / args.benchmark / name
        records: dict[str, dict] = {}
        for p in sorted(cfg_dir.glob("*.json")) if cfg_dir.exists() else []:
            if p.name == "summary.json":
                continue
            rec = json.loads(p.read_text(encoding="utf-8"))
            iid = rec["instance_id"]
            if want and iid not in want:
                continue
            if limit_ids is not None and iid not in limit_ids:
                continue
            records[iid] = rec
        if not records:
            print(f"[eval] {name}: no saved results to score", flush=True)
            continue

        predictions = {iid: rec.get("patch", "") for iid, rec in records.items()}
        instances = benchmark.load(list(records))
        eval_results = benchmark.evaluate(predictions, instances, f"{run_id}--{name}")
        for iid, rec in records.items():
            ev = eval_results.get(iid)
            rec["resolved"] = bool(ev.resolved) if ev else False
            rec["eval_report"] = ev.report if ev else {}
            rec["error"] = ev.error if ev else rec.get("error")
            results.save_result(rec)
            print(f"[eval] {name} :: {iid} :: resolved={rec['resolved']}", flush=True)
        summary = results.summarize(args.benchmark, name)
        print(f"[eval] summary {name}: {summary['resolved']}/{summary['instances']} resolved",
              flush=True)
    return 0


def cmd_summarize(args) -> int:
    defaults, configs = load_matrix(args.matrix)
    names = args.config or list(configs)
    for name in names:
        summary = results.summarize(args.benchmark, name)
        print(json.dumps(summary, indent=2))
    return 0


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(prog="yoloeval", description=__doc__)
    sub = p.add_subparsers(dest="command", required=True)

    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--benchmark", default="swebench_verified")
    common.add_argument("--matrix", default=None, help="path to config matrix YAML")
    common.add_argument("--config", action="append", help="config name(s) to run; repeatable")

    r = sub.add_parser("run", parents=[common], help="run the agent x instance matrix")
    g = r.add_mutually_exclusive_group()
    g.add_argument("--instance", action="append", help="specific instance id(s); repeatable")
    g.add_argument("--limit", type=int, help="run the first N instances")
    g.add_argument("--suite", help="named instance suite from bench/suites/ (e.g. tracking-v1)")
    r.add_argument("--run-id", default=None)
    r.add_argument("--max-cost", type=float, default=None,
                   help="per-instance USD cap (overrides matrix); kills the run if exceeded")
    r.add_argument("--no-eval", action="store_true", help="skip Docker evaluation (plumbing test)")
    r.add_argument("--keep-workdirs", action="store_true", help="don't delete checkouts after each run")
    r.add_argument("--max-workers", type=int, default=4, help="Docker eval workers")
    r.add_argument("--namespace", default="swebench", help="image namespace ('none' to build locally)")
    r.add_argument("--eval-timeout", type=int, default=1800, help="per-instance test timeout (s)")
    r.set_defaults(func=cmd_run)

    e = sub.add_parser("eval", parents=[common],
                       help="re-score saved patches without re-running the agent")
    eg = e.add_mutually_exclusive_group()
    eg.add_argument("--instance", action="append", help="specific instance id(s); repeatable")
    eg.add_argument("--limit", type=int, help="score the first N instances")
    eg.add_argument("--suite", help="named instance suite from bench/suites/ (e.g. tracking-v1)")
    e.add_argument("--run-id", default=None)
    e.add_argument("--max-workers", type=int, default=4, help="Docker eval workers")
    e.add_argument("--namespace", default="swebench", help="image namespace ('none' to build locally)")
    e.add_argument("--eval-timeout", type=int, default=1800, help="per-instance test timeout (s)")
    e.set_defaults(func=cmd_eval)

    s = sub.add_parser("summarize", parents=[common], help="rebuild summary.json files")
    s.set_defaults(func=cmd_summarize)

    return p


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
