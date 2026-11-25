#!/usr/bin/env python3
"""Analyze the matched pilot using held-out correctness, not aggregate scores."""
import argparse
import csv
import json
import math
from pathlib import Path
import random
import statistics

VALIDATIONS = ('cargo test', 'cargo check', 'cargo clippy', 'pytest', 'python -m unittest',
               'python3 -m unittest', 'npm test', 'pnpm test', 'go test')
EDITS = {'edit_file', 'write_file', 'apply_patch', 'delete_file', 'ast_edit'}


def observed_validations(calls):
    epoch = count = repeated = 0
    seen = set()
    for call in calls:
        name = call.get('name')
        if name in EDITS:
            epoch += 1
        if name != 'bash':
            continue
        command = ' '.join(call.get('arguments', {}).get('command', '').split())
        if any(marker in command for marker in VALIDATIONS):
            count += 1
            key = (epoch, command)
            if call.get('success') is True:
                repeated += key in seen
                seen.add(key)
        else:
            epoch += 1  # opaque shell work can mutate the fixture
    return count, repeated


def percentile(values, q):
    values = sorted(values)
    position = (len(values) - 1) * q
    lo, hi = math.floor(position), math.ceil(position)
    return values[lo] + (values[hi] - values[lo]) * (position - lo)


def oracle_result(case, oracle):
    t = case['transcript']
    corrected = False
    if (case['sample'] == 'continuity-validation' and not oracle['pass']
            and oracle.get('reason') == 'AssertionError'
            and t['final_response'].strip().endswith('VALIDATION_COMPLETE')):
        calls = t.get('metadata', {}).get('tool_invocations', [])
        checks = [c for c in calls if c.get('name') == 'bash']
        # Verifier v1 compiled and executed the held-out Rust check before its
        # bare suffix assertion. Retain its raw failure and correct only this
        # demonstrated wording bug, symmetrically for both arms.
        corrected = bool(checks) and not any(c.get('name') in EDITS for c in calls) and all(
            c.get('success') is True and c.get('arguments', {}).get('command', '').strip().startswith('cargo test')
            for c in checks)
    return bool(oracle['pass'] or corrected), corrected


def row(case):
    t = case['transcript']
    m = t.get('metrics', {})
    scores = {s['scorer']: s for s in case['scores']}
    oracle = scores.get('independent_correctness')
    if oracle is None or oracle.get('na') or case.get('skipped') or t.get('error_kind') == 'infra':
        raise ValueError('unscored/invalid execution cannot become a benchmark result')
    verified, correction = oracle_result(case, oracle)
    total_input = m.get('cumulative_input_tokens', t['usage']['input_tokens'])
    validation, repeated = observed_validations(t.get('metadata', {}).get('tool_invocations', []))
    return {'sample': case['sample'], 'arm': case['params']['binary'], 'trial': case.get('trial', 0),
            'verified': bool(verified and scores['succeeded']['pass']),
            'raw_oracle_pass': oracle['pass'], 'marker_oracle_correction': correction,
            'oracle_reason': oracle.get('reason', ''), 'agent_error': t.get('error'),
            'latency_ms': t['timing']['duration_ms'], 'cost_usd': t['usage']['cost_usd'],
            'input_tokens': total_input, 'output_tokens': t['usage']['output_tokens'],
            'cache_read_tokens': t['usage']['cache_read_tokens'],
            'first_request_tokens': m.get('first_request_total_input_tokens', 0),
            'llm_calls': m.get('llm_calls', t['iterations']), 'tool_calls': t['tool_calls_count'],
            'failed_tools': m.get('tool_calls_failed', 0),
            'failed_edits': m.get('edit_tool_calls_failed', 0),
            'observed_validations': validation, 'repeated_checks_between_observed_edits': repeated,
            'reported_validation_count': m.get('validation_tool_calls', 0)}


def summarize(rows):
    result = {}
    for arm in ('baseline', 'candidate'):
        selected = [r for r in rows if r['arm'] == arm]
        successes = sum(r['verified'] for r in selected)
        cost = sum(r['cost_usd'] for r in selected)
        total_input = sum(r['input_tokens'] for r in selected)
        result[arm] = {'cases': len(selected), 'verified': successes,
                       'cost_usd': cost, 'cost_per_verified_success_usd': cost / successes if successes else None,
                       'latency_p50_ms': statistics.median(r['latency_ms'] for r in selected),
                       'latency_p95_ms': percentile([r['latency_ms'] for r in selected], .95),
                       'cache_read_fraction': sum(r['cache_read_tokens'] for r in selected) / total_input if total_input else None}
        for key in ('input_tokens', 'output_tokens', 'llm_calls', 'tool_calls', 'failed_tools', 'failed_edits',
                    'observed_validations', 'repeated_checks_between_observed_edits'):
            result[arm][key] = sum(r[key] for r in selected)
    return result


def paired_ratios(rows, key):
    ratios = []
    per_task = []
    for sample in sorted({r['sample'] for r in rows}):
        means = {arm: statistics.mean(r[key] for r in rows if r['sample'] == sample and r['arm'] == arm)
                 for arm in ('baseline', 'candidate')}
        ratio = means['candidate'] / means['baseline'] if means['baseline'] else None
        per_task.append({'sample': sample, **means, 'candidate_over_baseline': ratio})
        if ratio and ratio > 0:
            ratios.append(math.log(ratio))
    rng = random.Random(20261124)
    draws = [math.exp(statistics.mean(rng.choices(ratios, k=len(ratios)))) for _ in range(10000)]
    return {'geometric_mean_task_ratio': math.exp(statistics.mean(ratios)),
            'task_cluster_bootstrap_95_percent_interval': [percentile(draws, .025), percentile(draws, .975)],
            'tasks': per_task}


def analyze(root):
    root = Path(root)
    manifest = json.loads((root / 'manifest.json').read_text())
    rows = []
    for run in manifest['runs']:
        rows.extend(row(c) for c in json.loads((root / run['report']).read_text())['cases'])
    expected = manifest['trials_per_arm_per_task']
    for sample in manifest['sample_order']:
        for arm in ('baseline', 'candidate'):
            selected = [r for r in rows if r['sample'] == sample and r['arm'] == arm]
            if len(selected) != expected or len({r['trial'] for r in selected}) != expected:
                raise ValueError(f'incomplete matched cell {sample}/{arm}')
    summary = summarize(rows)
    return {'method': manifest['method'], 'identities': manifest['identities'],
            'cost_scope': 'Recorded main-generation cost only. Everruns 0.53.1 StoreCommandHost::completion discards completion-review usage; totals are not full billed cost.',
            'summary': summary, 'cost': paired_ratios(rows, 'cost_usd'),
            'latency': paired_ratios(rows, 'latency_ms'), 'rows': rows,
            'limitations': ['Ten task clusters and three repetitions are a pilot, not a general agent leaderboard.',
                            'The seed controls the schedule, not provider sampling. Provider caches cannot be reset.',
                            'Empirical p95 has only 30 observations per arm and is not a stable tail estimate.',
                            'The original validation counter misses python3 unittest; observed counts are recomputed from structured calls.',
                            'Verifier v1 required a whole-response marker where the prompt required a suffix. The correction is explicit per row; original raw reports and verifier are retained.',
                            'Recorded cost excludes completion-review usage and is not total billed cost.',
                            'Independent correctness is primary. Legacy lexical and arm-specific efficiency scores do not define correctness.',
                            'Repeated-check counts track observed tool edits and opaque shell work, not arbitrary external mutations.']}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('root', type=Path)
    args = parser.parse_args()
    data = analyze(args.root)
    (args.root / 'analysis.json').write_text(json.dumps(data, indent=2))
    with (args.root / 'cases.csv').open('w') as stream:
        writer = csv.DictWriter(stream, fieldnames=list(data['rows'][0]), lineterminator='\n')
        writer.writeheader(); writer.writerows(data['rows'])
    lines = ['# Matched task-continuity pilot', '', data['cost_scope'], '', '| Metric | Baseline | Candidate |', '|---|---:|---:|']
    for key in data['summary']['baseline']:
        a, b = (data['summary'][arm][key] for arm in ('baseline', 'candidate'))
        lines.append(f'| {key} | {a} | {b} |')
    for metric in ('cost', 'latency'):
        block = data[metric]
        lines.extend(['', f"{metric}: paired task ratio {block['geometric_mean_task_ratio']:.3f}, task-cluster bootstrap interval {block['task_cluster_bootstrap_95_percent_interval']}."])
    lines.extend(['', '## Limits', ''] + ['- ' + item for item in data['limitations']])
    (args.root / 'analysis.md').write_text('\n'.join(lines) + '\n')
    print(json.dumps({k: v for k, v in data.items() if k in ('summary', 'cost', 'latency')}, indent=2))


if __name__ == '__main__':
    main()
