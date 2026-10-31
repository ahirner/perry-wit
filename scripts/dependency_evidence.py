#!/usr/bin/env python3
"""Compare concrete conformance evidence and prepare the dependency PR report."""

import argparse
import hashlib
import json
from pathlib import Path

from ci_checks import CHECKS, write_json

SHARDS = 4
EVIDENCE = Path('dependency-evidence')


def case_id(saved):
    concrete = {key: saved[key] for key in ('contract', 'case', 'source', 'metamorphic')}
    concrete['limits'] = saved['settings']['limits']
    concrete['depth'] = saved['settings']['depth']
    return hashlib.sha256(
        json.dumps(concrete, sort_keys=True, separators=(',', ':'), ensure_ascii=False).encode()
    ).hexdigest()


def discover_cases(results):
    cases = {}
    for path in sorted(results.glob('results-campaign-*/evidence/run-*/*/*.json')):
        if path.name not in ('original.json', 'minimal.json', 'failure.json'):
            continue
        saved = json.loads(path.read_text())
        identity = case_id(saved)
        cases.setdefault(identity, (path, saved))
    return cases


def classify(before, after):
    return {
        ('passed', 'failed'): 'regression',
        ('failed', 'failed'): 'existing failure',
        ('failed', 'passed'): 'resolved failure',
        ('passed', 'passed'): 'not reproduced',
    }.get((before, after), 'inconclusive')


def read_result(path):
    try:
        return json.loads(path.read_text())
    except (FileNotFoundError, json.JSONDecodeError):
        return {}


def compare(metadata, results):
    blockers = list(metadata['errors'])
    contracts = {'baseline': {}, 'candidate': {}}
    replay_results = {}
    for phase in contracts:
        build_directory = results / f'results-build-{phase}'
        build_report = read_result(build_directory / 'build.json')
        catalog = read_result(build_directory / 'catalog.json')
        if not (
            build_report.get('completed')
            and build_report.get('source_unchanged')
            and build_report.get('revision') == metadata[phase]
        ):
            blockers.append(f'{phase}: build incomplete or source changed')
        expected = {entry['id'] for entry in catalog} if isinstance(catalog, list) else set()
        if not expected:
            blockers.append(f'{phase}: missing contract catalog')
        for shard in range(SHARDS):
            directory = results / f'results-campaign-{phase}-{shard}'
            campaign_report = read_result(directory / 'campaign.json')
            report = read_result(directory / 'evidence' / 'report.json')
            valid = (
                report.get('schema') == 2
                and report.get('completed')
                and report.get('source_unchanged')
                and report.get('revision') == metadata[phase]
                and report.get('shard') == {'index': shard, 'count': SHARDS}
                and all(report.get('settings', {}).get(key) == value for key, value in metadata['settings'].items())
                and campaign_report.get('status') in ('passed', 'failed')
                and campaign_report.get('exit_code') in (0, 1)
            )
            if not valid:
                blockers.append(f'{phase} shard {shard}: missing, incomplete, or mismatched campaign')
            for contract in report.get('contracts', []):
                name = contract['id']
                assigned = int.from_bytes(hashlib.sha256(name.encode()).digest()[:8], 'big') % SHARDS
                if name in contracts[phase] or assigned != shard:
                    blockers.append(f'{phase}: duplicate or misplaced contract {name}')
                contracts[phase][name] = contract
                if contract['status'] in ('not_run', 'running'):
                    blockers.append(f'{phase}: {name} did not finish')
                if contract['status'] not in (
                    'passed',
                    'failed',
                    'pending',
                    'deferred',
                    'newly_passing',
                    'not_run',
                    'running',
                ):
                    blockers.append(f'{phase}: {name} has an unknown status')
                if phase == 'candidate' and contract['status'] in ('failed', 'newly_passing'):
                    blockers.append(f'candidate: {name}: {contract["status"]}')
            if set(contracts[phase]) - expected:
                blockers.append(f'{phase}: unexpected contracts in campaign')
        if set(contracts[phase]) != expected:
            blockers.append(f'{phase}: incomplete contract coverage')
        replay_report = read_result(results / f'results-replay-{phase}' / 'replays.json')
        if not replay_report.get('completed') or replay_report.get('revision') != metadata[phase]:
            blockers.append(f'{phase}: replay validation incomplete')
        replay_results[phase] = replay_report.get('cases', {})
    cases = []
    for identity, (_, saved) in discover_cases(results).items():
        before = replay_results['baseline'].get(identity, {}).get('outcome', 'inconclusive')
        after = replay_results['candidate'].get(identity, {}).get('outcome', 'inconclusive')
        classification = classify(before, after)
        required = any(contracts[phase].get(saved['contract'], {}).get('expected_pass', False) for phase in contracts)
        cases.append(
            {
                'id': identity,
                'contract': saved['contract'],
                'classification': classification,
                'baseline': before,
                'candidate': after,
                'expected_pass': required,
            }
        )
        if classification == 'inconclusive' or (required and classification != 'resolved failure'):
            blockers.append(f'{saved["contract"]}: {classification} ({identity[:12]})')
    for name, contract in contracts['baseline'].items():
        if contract['status'] == 'failed' and not any(case['contract'] == name for case in cases):
            blockers.append(f'baseline: {name}: failure has no replayable case')
    suite = read_result(results / 'results-suite' / 'suite.json')
    expected_checks = {name for name, _ in CHECKS}
    if not suite.get('completed') or set(suite.get('checks', {})) != expected_checks:
        blockers.append('standard suite incomplete')
    for name, check in suite.get('checks', {}).items():
        if check.get('status') != 'passed':
            blockers.append(f'standard suite: {name}: {check.get("status")}')
    suite_identity = read_result(results / 'results-suite' / 'identity.json')
    if suite_identity.get('revision') != metadata['candidate'] or not suite_identity.get('source_unchanged'):
        blockers.append('standard suite snapshot missing or changed')
    return {
        'ready': not blockers,
        'blockers': sorted(set(blockers)),
        'cases': cases,
        'contracts': contracts,
        'suite': suite,
    }


def retain_evidence(metadata, comparison, results):
    added = []
    summaries = {case['id']: case for case in comparison['cases']}
    for identity, (_, saved) in discover_cases(results).items():
        directory = EVIDENCE / identity
        if directory.exists():
            continue
        directory.mkdir(parents=True)
        write_json(directory / 'case.json', saved)
        (directory / 'case.ts').write_text(saved['source'])
        for index, variant in enumerate(saved['metamorphic']):
            (directory / f'form-{index}.ts').write_text(variant['source'])
        write_json(
            directory / 'comparison.json',
            {
                **summaries[identity],
                'baseline_revision': metadata['baseline'],
                'candidate_revision': metadata['candidate'],
                'run_url': metadata['run_url'],
            },
        )
        (directory / 'README.md').write_text(
            f'# {saved["contract"]}\n\n'
            'Review evidence; this case is not automatically registered as a test. '
            'A minimized case is the best found within the configured shrink budget.\n\n'
            f'Replay on either recorded revision, keeping this case outside the checkout if necessary:\n\n'
            f'```sh\nnix develop -c cargo run --locked -p perry-conformance -- replay {directory}/case.json\n```\n'
        )
        added.append(directory)
    return added


def summary(metadata, comparison):
    lines = [
        '## Dependency update',
        '',
        f'[Workflow results and artifacts]({metadata["run_url"]})',
        '',
        f'Baseline: `{metadata["baseline"]}` (update branch before this refresh).',
        f'Candidate: `{metadata["candidate"]}`.',
        '',
        'Campaign: '
        + ', '.join(f'{key}={value}' for key, value in metadata['settings'].items())
        + f'; {SHARDS} shards per snapshot.',
        '',
    ]
    for name, upstream in metadata.get('upstreams', {}).items():
        lines.append(
            f'- [{name}: {upstream["before"][:12]} → {upstream["after"][:12]}]({upstream["url"]}/compare/{upstream["before"]}...{upstream["after"]})'
        )
    lines.extend(
        [
            '',
            '### Validation',
            '',
            'Passed updater validation; ordinary PR CI remains the merge gate.'
            if comparison['ready']
            else 'Draft: validation failed or is incomplete.',
            '',
        ]
    )
    lines.extend(f'- {blocker}' for blocker in comparison['blockers'])
    for name, check in comparison['suite'].get('checks', {}).items():
        if check.get('status') != 'passed':
            lines.extend(['', f'Failed command ({name}):', '', '```sh', ' '.join(check.get('command', [name])), '```'])
    if comparison['cases']:
        lines.extend(['', '| Contract | Concrete replay | Case |', '| --- | --- | --- |'])
        for case in comparison['cases']:
            lines.append(f'| `{case["contract"]}` | {case["classification"]} | `{case["id"][:12]}` |')
    for phase, contracts in comparison['contracts'].items():
        failures = [
            (name, entry)
            for name, entry in contracts.items()
            if entry['status'] in ('failed', 'newly_passing', 'pending')
        ]
        if failures:
            lines.extend(['', f'### {phase.capitalize()} diagnostics', ''])
            for name, entry in failures:
                diagnostic = (entry.get('error') or entry['status'])[:1200].replace('```', "'''")
                lines.extend([f'**{name}** ({entry["status"]})', '', '```text', diagnostic, '```', ''])
    lines.extend(
        [
            '',
            'Counterexamples are review evidence only. Minimized cases are bounded shrink results. '
            'Detailed command logs, generated binaries, and replay reports are attached to the workflow.',
        ]
    )
    return '\n'.join(lines)[:60000] + '\n'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', type=Path, default=Path('artifacts/source'))
    parser.add_argument('--results', type=Path, default=Path('artifacts/results'))
    parser.add_argument('--output', type=Path, default=Path('artifacts/comparison'))
    args = parser.parse_args()
    metadata = json.loads((args.source / 'metadata.json').read_text())
    comparison = compare(metadata, args.results)
    retain_evidence(metadata, comparison, args.results)
    write_json(args.output / 'comparison.json', comparison)
    (args.output / 'summary.md').write_text(summary(metadata, comparison))


if __name__ == '__main__':
    main()
