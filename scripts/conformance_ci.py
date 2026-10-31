#!/usr/bin/env python3
"""Run bounded campaigns and replay their counterexamples using a prebuilt runner."""

import argparse
import json
from pathlib import Path
import shutil
import subprocess
import tarfile
import time

from ci_checks import run_logged, write_json
from dependency_evidence import SHARDS, discover_cases


def unpack_runner(binary):
    executable = binary / 'perry-conformance'
    with tarfile.open(binary / 'runner.tar') as archive:
        with archive.extractfile('perry-conformance') as source, executable.open('wb') as target:
            shutil.copyfileobj(source, target)
    executable.chmod(0o755)
    return executable.resolve()


def campaign(output, metadata, phase, shard, binary):
    executable = unpack_runner(binary)
    settings = metadata['settings']
    output.mkdir(parents=True, exist_ok=True)
    command = [
        'nix',
        'develop',
        '-c',
        str(executable),
        'check',
        '--shard',
        f'{shard}/{SHARDS}',
        '--output-dir',
        str(output / 'evidence'),
    ]
    for key in ('seed', 'cases', 'depth', 'shrink_limit'):
        command.extend(['--' + key.replace('_', '-'), str(settings[key])])
    result = run_logged(command, output / 'campaign.log', 600)
    result.update(
        phase=phase, shard=shard, revision=subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
    )
    write_json(output / 'campaign.json', result)
    return int(result['status'] != 'passed')


def replay(output, results, phase, binary):
    executable = unpack_runner(binary)
    report = {
        'revision': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
        'completed': False,
        'cases': {},
    }
    output.mkdir(parents=True, exist_ok=True)
    write_json(output / 'replays.json', report)
    deadline = time.monotonic() + 300
    for identity, (path, _) in discover_cases(results).items():
        directory = output / identity
        result = run_logged(
            [str(executable), 'replay', str(path), '--output-dir', str(directory)],
            output / f'{identity}.log',
            min(120, deadline - time.monotonic()),
        )
        reports = list(directory.glob('replay-*/replay.json'))
        result['outcome'] = 'inconclusive'
        if len(reports) == 1 and result['status'] in ('passed', 'failed'):
            evidence = json.loads(reports[0].read_text())
            if evidence['source_identity'][1] == report['revision']:
                result['outcome'] = evidence.get('outcome', 'inconclusive')
                result['error'] = evidence.get('error')
        report['cases'][identity] = result
        write_json(output / 'replays.json', report)
    report['completed'] = True
    write_json(output / 'replays.json', report)
    return int(any(case['outcome'] not in ('passed', 'failed') for case in report['cases'].values()))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['campaign', 'replay'])
    parser.add_argument('--source', type=Path, default=Path('artifacts/source'))
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--results', type=Path, default=Path('artifacts/results'))
    parser.add_argument('--binary', type=Path, default=Path('artifacts/binary'))
    parser.add_argument('--phase', choices=['baseline', 'candidate'], required=True)
    parser.add_argument('--shard', type=int, choices=range(SHARDS), default=0)
    args = parser.parse_args()
    metadata = json.loads((args.source / 'metadata.json').read_text())
    revision = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
    if revision != metadata[args.phase]:
        raise ValueError('checkout does not match the recorded snapshot')
    if args.action == 'campaign':
        return campaign(args.output.resolve(), metadata, args.phase, args.shard, args.binary)
    return replay(args.output.resolve(), args.results.resolve(), args.phase, args.binary)


if __name__ == '__main__':
    raise SystemExit(main())
