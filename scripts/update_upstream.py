#!/usr/bin/env python3
"""Advance explicit Perry and pliron revision pins to their default-branch heads."""

import json
from pathlib import Path
import re
import subprocess
import tomllib

from ci_checks import write_json

UPSTREAMS = {
    'perry': ('https://github.com/PerryTS/perry', ['perry-hir', 'perry-parser']),
    'pliron': ('https://github.com/pliron-org/pliron', ['pliron']),
}


def manifest_with_revisions(source, revisions):
    dependencies = tomllib.loads(source)['workspace']['dependencies']
    for upstream, revision in revisions.items():
        if not re.fullmatch(r'[0-9a-f]{40}', revision):
            raise ValueError(f'invalid {upstream} revision')
        url, names = UPSTREAMS[upstream]
        for name in names:
            dependency = dependencies[name]
            if dependency.get('git') != url or 'rev' not in dependency:
                raise ValueError(f'expected an explicit revision for {name} from {url}')
            pattern = rf'(?m)^({re.escape(name)}\s*=\s*\{{[^\n]*?\brev\s*=\s*"){re.escape(dependency["rev"])}(")'
            source, count = re.subn(pattern, lambda match: match[1] + revision + match[2], source)
            if count != 1:
                raise ValueError(f'cannot unambiguously update {name}')
    updated = tomllib.loads(source)['workspace']['dependencies']
    for upstream, revision in revisions.items():
        if any(updated[name]['rev'] != revision for name in UPSTREAMS[upstream][1]):
            raise ValueError(f'inconsistent {upstream} revisions')
    return source


def main():
    manifest = Path('Cargo.toml')
    source = manifest.read_text()
    dependencies = tomllib.loads(source)['workspace']['dependencies']
    upstreams = {}
    for name, (url, packages) in UPSTREAMS.items():
        head = subprocess.check_output(['git', 'ls-remote', '--symref', url, 'HEAD'], text=True, timeout=60)
        match = re.search(r'(?m)^([0-9a-f]{40})\s+HEAD$', head)
        if not match:
            raise ValueError(f'cannot resolve {name} default-branch head')
        upstreams[name] = {'url': url, 'before': dependencies[packages[0]]['rev'], 'after': match[1]}
    updated = manifest_with_revisions(source, {name: entry['after'] for name, entry in upstreams.items()})
    manifest.write_text(updated)
    write_json(Path('artifacts/source/upstreams.json'), upstreams)
    print(json.dumps(upstreams, indent=2))


if __name__ == '__main__':
    main()
