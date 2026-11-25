#!/usr/bin/env python3
"""Counterbalanced, matched-binary pilot using the existing Mira study."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import random
import re
import subprocess
import time

SAMPLES = ['add-fn', 'fix-off-by-one', 'rename-across-files', 'implement-todo',
           'find-constant', 'expected-diagnostic-failure-control', 'continuity-validation',
           'untrusted-file-instructions', 'continuity-independent-read', 'continuity-late-edit']


def digest(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def resume_manifest(path, expected):
    if not path.exists():
        return expected
    previous = json.loads(path.read_text())
    if {k: v for k, v in previous.items() if k != 'runs'} != json.loads(json.dumps(
            {k: v for k, v in expected.items() if k != 'runs'})):
        raise ValueError('existing manifest differs from this experiment; use a new output directory')
    for entry in previous['runs']:
        if not (path.parent / entry['report']).is_file():
            raise ValueError('a completed run is missing its report; restore it before resuming')
    return previous


def save_manifest(path, manifest):
    temporary = path.with_suffix('.json.tmp')
    temporary.write_text(json.dumps(manifest, indent=2))
    temporary.replace(path)


def main():
    parser = argparse.ArgumentParser()
    for flag in ('mira', 'study', 'baseline', 'candidate', 'baseline_source', 'candidate_source', 'out'):
        parser.add_argument('--' + flag.replace('_', '-'), required=True, type=Path)
    parser.add_argument('--trials', type=int, default=3)
    parser.add_argument('--seed', type=int, default=20261124)
    parser.add_argument('--client-date', required=True, help='Caller date in their timezone, YYYY-MM-DD')
    args = parser.parse_args()
    if args.trials < 1:
        parser.error('--trials must be positive')
    root = args.out.resolve()
    root.mkdir(parents=True, exist_ok=True)
    binaries = {arm: getattr(args, arm).resolve() for arm in ('baseline', 'candidate')}
    assert digest(binaries['baseline']) != digest(binaries['candidate']), 'binary artifacts must differ'
    identities = {}
    for arm in binaries:
        source = getattr(args, arm + '_source').resolve()
        identities[arm] = {'commit': subprocess.check_output(['git', '-C', str(source), 'rev-parse', 'HEAD'], text=True).strip(),
                           'binary_sha256': digest(binaries[arm]),
                           'cargo_lock_sha256': digest(source / 'Cargo.lock'),
                           'toolchain_sha256': digest(source / 'rust-toolchain.toml')}
    for field in ('cargo_lock_sha256', 'toolchain_sha256'):
        assert identities['baseline'][field] == identities['candidate'][field], field
    samples = SAMPLES.copy()
    random.Random(args.seed).shuffle(samples)
    plan = [(sample, arm) for index, sample in enumerate(samples)
            for arm in (('baseline', 'candidate') if index % 2 == 0 else ('candidate', 'baseline'))]
    manifest = {'client_date': args.client_date, 'model': 'meta/muse-spark-1.3-contributor',
                'identities': identities, 'trials_per_arm_per_task': args.trials,
                'seed': args.seed, 'sample_order': samples, 'plan': plan,
                'concurrency': 2, 'provider_sampling_seed_supported': False,
                'timeout_seconds': 240, 'timeouts_are_not_a_spend_cap': True,
                'method': f'Alternating arm order across 10 seeded tasks; {args.trials} fresh sessions per arm/task. Provider cache cannot be reset; cache tokens are reported. Primary quality is independent_correctness, separate from legacy shape/efficiency scores.',
                'harness_sha256': {name: digest(path) for name, path in {
                    'mira': args.mira, 'study': args.study,
                    'verifier': Path(__file__).resolve().with_name('verify_coding_result.py'),
                    'runner': Path(__file__).resolve()}.items()},
                'runs': []}
    path = root / 'manifest.json'
    manifest = resume_manifest(path, manifest)
    save_manifest(path, manifest)
    env = os.environ.copy()
    if not env.get('OPENROUTER_API_KEY'):
        raise SystemExit('OPENROUTER_API_KEY is required; do not accept skipped cases')
    env.update({'HARNESS_BASIC_BASELINE_BIN': str(binaries['baseline']),
                'HARNESS_BASIC_CANDIDATE_BIN': str(binaries['candidate']),
                'HARNESS_BASIC_TIMEOUT_S': '235', 'NO_COLOR': '1',
                'RUSTUP_HOME': env.get('RUSTUP_HOME', str(Path.home() / '.rustup')),
                'RUSTUP_TOOLCHAIN': '1.100.0'})
    cwd = Path(__file__).resolve().parent
    for index, (sample, arm) in enumerate(plan):
        folder = root / f'{index:02}-{sample}-{arm}'
        folder.mkdir(exist_ok=True)
        report = folder / 'report.json'
        registered = any(entry['report'] == str(report.relative_to(root)) for entry in manifest['runs'])
        if registered:
            continue
        if report.exists():
            raise RuntimeError(f'unregistered report {report}; recover its manifest entry before resuming')
        command = [str(args.mira.resolve()), 'run', '--study-cmd', str(args.study.resolve()),
                   '--samples', sample, '--targets', 'openrouter/muse-spark-1.3',
                   '--axis', f'binary={arm}', '--axis', 'harness=default', '--axis', 'effort=default',
                   '--trials', str(args.trials), '--seed', str(args.seed),
                   '--max-concurrent', '2', '--provider-concurrency', 'openrouter=2',
                   '--no-adaptive', '--max-retries', '0', '--timeout', '240', '--out', str(report)]
        start = time.monotonic()
        with (folder / 'run.log').open('w') as log:
            result = subprocess.run(command, cwd=cwd, env=env, stdout=log, stderr=subprocess.STDOUT,
                                    timeout=args.trials * 240 + 120)
        if not report.exists():
            raise RuntimeError(f'no report for {sample}/{arm}; inspect run.log')
        data = json.loads(report.read_text())
        cases = data['cases']
        assert len(cases) == args.trials, 'unexpected selection or skipped repetitions'
        assert all(not c.get('skipped') and c['transcript'].get('error_kind') != 'infra' for c in cases), 'invalid benchmark execution'
        verified = sum(next((s['pass'] for s in c['scores'] if s['scorer'] == 'independent_correctness'), False) for c in cases)
        run_id = re.search(r'saved run (\S+) to', (folder / 'run.log').read_text())
        entry = {'sample': sample, 'arm': arm, 'verified': verified, 'cases': len(cases),
                 'exit_code': result.returncode, 'seconds': round(time.monotonic() - start, 2),
                 'run_id': run_id.group(1) if run_id else None,
                 'report': str(report.relative_to(root))}
        manifest['runs'].append(entry)
        save_manifest(path, manifest)
        print(json.dumps(entry), flush=True)
    print(json.dumps({'complete': True, 'runs': len(manifest['runs'])}), flush=True)


if __name__ == '__main__':
    main()
