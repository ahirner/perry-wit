#!/usr/bin/env python3
"""Run the shared CI suite and retain results when individual checks fail."""

import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import time


CHECKS = [
    ('runtime-fetch', ['cargo', 'fetch', '--manifest-path', 'runtime/Cargo.toml', '--locked']),
    ('workspace-tests', ['cargo', 'test', '--workspace', '--locked', '--no-fail-fast']),
    ('benchmarks', ['cargo', 'test', '--locked', '--test', 'benchmark', '--', '--ignored', '--nocapture']),
    (
        'runtime-tests',
        ['cargo', 'test', '--manifest-path', 'runtime/Cargo.toml', '--workspace', '--locked', '--no-fail-fast'],
    ),
    (
        'runtime-features',
        ['cargo', 'test', '--manifest-path', 'runtime/Cargo.toml', '-p', 'perry-runtime', '--all-features', '--locked'],
    ),
    (
        'runtime-clippy',
        [
            'cargo',
            'clippy',
            '--manifest-path',
            'runtime/Cargo.toml',
            '--workspace',
            '--all-targets',
            '--all-features',
            '--locked',
            '--',
            '-D',
            'warnings',
        ],
    ),
    (
        'workspace-clippy',
        ['cargo', 'clippy', '--workspace', '--all-targets', '--all-features', '--locked', '--', '-D', 'warnings'],
    ),
]


def write_json(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + '.tmp')
    temporary.write_text(json.dumps(value, indent=2) + '\n')
    temporary.replace(path)


def run_logged(command, log, timeout, *, cwd=None):
    log = Path(log)
    log.parent.mkdir(parents=True, exist_ok=True)
    started = time.monotonic()
    result = {'command': command, 'status': 'timed_out', 'exit_code': None}
    print('Running: ' + ' '.join(command), flush=True)
    with log.open('w') as output:
        if timeout > 0:
            try:
                process = subprocess.Popen(
                    command,
                    cwd=cwd,
                    stdout=output,
                    stderr=subprocess.STDOUT,
                    start_new_session=True,
                )
                try:
                    result['exit_code'] = process.wait(timeout=timeout)
                    result['status'] = 'passed' if process.returncode == 0 else 'failed'
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGTERM)
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.wait()
            except OSError as error:
                output.write(str(error) + '\n')
                result['status'] = 'error'
    result['seconds'] = round(time.monotonic() - started, 3)
    print(f'{result["status"]}: {result["seconds"]}s; log: {log}', flush=True)
    if result['status'] != 'passed':
        print('\n'.join(log.read_text(errors='replace').splitlines()[-80:]), flush=True)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output-dir', type=Path, default=Path('target/ci'))
    parser.add_argument('--conformance', action='store_true')
    parser.add_argument('--budget', type=int, default=2400)
    args = parser.parse_args()
    deadline = time.monotonic() + args.budget
    checks = list(CHECKS)
    if args.conformance:
        checks.append(('conformance', ['cargo', 'run', '--locked', '-p', 'perry-conformance', '--', 'check']))
    report = {'completed': False, 'checks': {}}
    report_path = args.output_dir / 'suite.json'
    for name, command in checks:
        report['checks'][name] = {'status': 'running', 'command': command}
        write_json(report_path, report)
        report['checks'][name] = run_logged(command, args.output_dir / f'{name}.log', deadline - time.monotonic())
        write_json(report_path, report)
    report['completed'] = True
    write_json(report_path, report)
    return int(any(check['status'] != 'passed' for check in report['checks'].values()))


if __name__ == '__main__':
    raise SystemExit(main())
