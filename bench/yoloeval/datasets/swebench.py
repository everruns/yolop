"""SWE-bench Verified benchmark.

Loading: the dataset is a single parquet on the Hugging Face hub. We download
it once into a local cache and read it with pandas so we don't depend on the
``datasets`` runtime at load time.

Evaluation: we shell out to the official ``swebench.harness.run_evaluation``,
which builds/pulls per-instance Docker images, applies the model patch and the
hidden ``test_patch``, runs ``FAIL_TO_PASS``/``PASS_TO_PASS``, and writes a
``report.json`` per instance. We parse those reports back into ``EvalResult``.
This keeps us aligned with the canonical scoring instead of reimplementing it.
"""

from __future__ import annotations

import json
import subprocess
import sys
import urllib.request
from pathlib import Path
from typing import Iterable, Mapping

from ..models import EvalResult, Instance
from .base import Benchmark

HF_REPO = "princeton-nlp/SWE-bench_Verified"
PARQUET_URL = (
    f"https://huggingface.co/datasets/{HF_REPO}"
    "/resolve/main/data/test-00000-of-00001.parquet"
)

# Fields copied verbatim into the evaluation dataset file; mirror the HF schema
# so the official harness reads them exactly as it would from the hub.
_DATASET_FIELDS = [
    "repo",
    "instance_id",
    "base_commit",
    "patch",
    "test_patch",
    "problem_statement",
    "hints_text",
    "created_at",
    "version",
    "FAIL_TO_PASS",
    "PASS_TO_PASS",
    "environment_setup_commit",
    "difficulty",
]

CACHE_DIR = Path(__file__).resolve().parents[2] / ".cache"


class SWEBenchVerified(Benchmark):
    name = "swebench_verified"

    def __init__(
        self,
        parquet_path: str | None = None,
        max_workers: int = 4,
        namespace: str = "swebench",
        timeout: int = 1800,
        python_bin: str | None = None,
    ):
        self.parquet_path = Path(parquet_path) if parquet_path else CACHE_DIR / "swebench_verified.parquet"
        self.max_workers = max_workers
        # "swebench" pulls prebuilt images from Docker Hub; "none" builds locally.
        self.namespace = namespace
        self.timeout = timeout
        # Use the venv interpreter that has swebench installed.
        self.python_bin = python_bin or sys.executable
        self._raw: dict[str, dict] = {}

    # -- loading -----------------------------------------------------------
    def _ensure_parquet(self) -> None:
        if self.parquet_path.exists():
            return
        self.parquet_path.parent.mkdir(parents=True, exist_ok=True)
        print(f"[swebench] downloading dataset -> {self.parquet_path}", flush=True)
        urllib.request.urlretrieve(PARQUET_URL, self.parquet_path)

    def load(self, instance_ids: list[str] | None = None) -> list[Instance]:
        import pandas as pd

        self._ensure_parquet()
        df = pd.read_parquet(self.parquet_path)
        if instance_ids:
            wanted = set(instance_ids)
            df = df[df["instance_id"].isin(wanted)]

        instances: list[Instance] = []
        for _, row in df.iterrows():
            raw = {k: _native(row[k]) for k in _DATASET_FIELDS if k in row}
            self._raw[raw["instance_id"]] = raw
            instances.append(
                Instance(
                    instance_id=raw["instance_id"],
                    problem_statement=raw["problem_statement"],
                    repo=raw["repo"],
                    base_commit=raw["base_commit"],
                    extra={
                        "version": raw.get("version"),
                        "difficulty": raw.get("difficulty"),
                        "environment_setup_commit": raw.get("environment_setup_commit"),
                    },
                )
            )
        return instances

    # -- evaluation --------------------------------------------------------
    def evaluate(
        self,
        predictions: Mapping[str, str],
        instances: Iterable[Instance],
        run_id: str,
    ) -> dict[str, EvalResult]:
        instances = list(instances)
        ids = [i.instance_id for i in instances]
        if not self._raw:
            # evaluate() called without a prior load(); reload the raw rows.
            self.load(ids)

        work = CACHE_DIR / "eval" / run_id
        work.mkdir(parents=True, exist_ok=True)
        dataset_file = work / "dataset.jsonl"
        preds_file = work / "predictions.jsonl"

        with dataset_file.open("w", encoding="utf-8") as fh:
            for iid in ids:
                fh.write(json.dumps(self._raw[iid]) + "\n")

        model_name = "yolop"
        with preds_file.open("w", encoding="utf-8") as fh:
            for iid in ids:
                fh.write(
                    json.dumps(
                        {
                            "instance_id": iid,
                            "model_name_or_path": model_name,
                            "model_patch": predictions.get(iid, ""),
                        }
                    )
                    + "\n"
                )

        cmd = [
            self.python_bin,
            "-m",
            "swebench.harness.run_evaluation",
            "--dataset_name", str(dataset_file),
            "--predictions_path", str(preds_file),
            "--run_id", run_id,
            "--instance_ids", *ids,
            "--max_workers", str(self.max_workers),
            "--namespace", self.namespace,
            "--timeout", str(self.timeout),
            "--report_dir", str(work),
        ]
        print(f"[swebench] running evaluation: {' '.join(cmd)}", flush=True)
        proc = subprocess.run(cmd, cwd=str(work))

        results: dict[str, EvalResult] = {}
        log_root = work / "logs" / "run_evaluation" / run_id / model_name
        for iid in ids:
            report_path = log_root / iid / "report.json"
            if not report_path.exists():
                results[iid] = EvalResult(
                    resolved=False,
                    error=f"no report.json (eval exit {proc.returncode}); see {log_root / iid}",
                )
                continue
            report = json.loads(report_path.read_text())
            entry = report.get(iid, {})
            results[iid] = EvalResult(
                resolved=bool(entry.get("resolved", False)),
                report=entry,
            )
        return results


def _native(value):
    """Convert numpy/pandas scalars to plain JSON-serializable Python values."""
    if hasattr(value, "item"):
        try:
            return value.item()
        except (ValueError, AttributeError):
            pass
    if hasattr(value, "tolist"):
        return value.tolist()
    return value
