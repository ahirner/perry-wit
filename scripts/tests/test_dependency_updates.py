import copy
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from ci_checks import CHECKS, run_logged, write_json
from dependency_evidence import case_id, compare, discover_cases, retain_evidence
from update_upstream import manifest_with_revisions


ROOT = Path(__file__).resolve().parents[2]
BRANCH_SCRIPT = ROOT / 'scripts/dependency_branch.sh'
CONTRACT = 'ecma.number.arithmetic'


class EvidenceTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.results = Path(self.directory.name)
        self.metadata = {
            'baseline': 'a' * 40,
            'candidate': 'b' * 40,
            'errors': [],
            'run_url': 'https://example.test/run',
            'settings': {'seed': 42, 'cases': 8, 'depth': 3, 'shrink_limit': 100},
        }
        self.shard = int.from_bytes(hashlib.sha256(CONTRACT.encode()).digest()[:8], 'big') % 4
        self.saved = {
            'contract': CONTRACT,
            'case': {'x': 1},
            'source': 'export function run() { return 1; }',
            'metamorphic': [{'case': {'x': 1}, 'source': 'export function run() { return 1 + 0; }'}],
            'settings': {**self.metadata['settings'], 'limits': {'fuel': 100}},
            'revision': 'a' * 40,
            'source_sha256': 'before',
            'versions': {'node': '26'},
        }
        for phase in ('baseline', 'candidate'):
            revision = self.metadata[phase]
            write_json(
                self.results / f'results-build-{phase}/build.json',
                {'completed': True, 'source_unchanged': True, 'revision': revision},
            )
            write_json(self.results / f'results-build-{phase}/catalog.json', [{'id': CONTRACT}])
            write_json(
                self.results / f'results-replay-{phase}/replays.json',
                {'completed': True, 'revision': revision, 'cases': {}},
            )
            for shard in range(4):
                directory = self.results / f'results-campaign-{phase}-{shard}'
                write_json(directory / 'campaign.json', {'status': 'passed', 'exit_code': 0})
                write_json(
                    directory / 'evidence/report.json',
                    {
                        'schema': 2,
                        'completed': True,
                        'source_unchanged': True,
                        'revision': revision,
                        'shard': {'index': shard, 'count': 4},
                        'settings': self.metadata['settings'],
                        'contracts': [{'id': CONTRACT, 'status': 'passed', 'expected_pass': True}]
                        if shard == self.shard
                        else [],
                    },
                )
        write_json(
            self.results / 'results-suite/suite.json',
            {'completed': True, 'checks': {name: {'status': 'passed'} for name, _ in CHECKS}},
        )
        write_json(
            self.results / 'results-suite/identity.json',
            {'revision': self.metadata['candidate'], 'source_unchanged': True},
        )

    def change_contract(self, phase, status, required=True):
        path = self.results / f'results-campaign-{phase}-{self.shard}/evidence/report.json'
        report = json.loads(path.read_text())
        report['contracts'][0].update(status=status, expected_pass=required)
        write_json(path, report)

    def add_case(self, before, after, name='original', saved=None):
        saved = saved or self.saved
        path = self.results / f'results-campaign-candidate-{self.shard}/evidence/run-test/{CONTRACT}/{name}.json'
        write_json(path, saved)
        identity = case_id(saved)
        for phase, outcome in (('baseline', before), ('candidate', after)):
            path = self.results / f'results-replay-{phase}/replays.json'
            report = json.loads(path.read_text())
            report['cases'][identity] = {'outcome': outcome}
            write_json(path, report)

    def test_complete_validation_requires_every_shard_and_exact_snapshot(self):
        self.assertTrue(compare(self.metadata, self.results)['ready'])
        report = self.results / 'results-campaign-baseline-0/evidence/report.json'
        original = json.loads(report.read_text())
        for patch in (
            {'completed': False},
            {'source_unchanged': False},
            {'revision': 'wrong'},
            {'settings': {'seed': 0}},
        ):
            write_json(report, original | patch)
            self.assertFalse(compare(self.metadata, self.results)['ready'], patch)
        report.unlink()
        self.assertFalse(compare(self.metadata, self.results)['ready'])

    def test_concrete_replay_distinguishes_regression_from_shrinking_to_existing_bug(self):
        self.change_contract('candidate', 'failed')
        self.add_case('passed', 'failed')
        minimal = copy.deepcopy(self.saved)
        minimal['source'] = 'export function run() { return 0; }'
        self.add_case('failed', 'failed', 'minimal', minimal)
        comparison = compare(self.metadata, self.results)
        self.assertFalse(comparison['ready'])
        self.assertEqual({case['classification'] for case in comparison['cases']}, {'regression', 'existing failure'})

    def test_resolved_failure_pending_contract_and_ratchet_remain_distinct(self):
        self.change_contract('baseline', 'failed')
        self.add_case('failed', 'passed')
        self.assertTrue(compare(self.metadata, self.results)['ready'])
        for phase in ('baseline', 'candidate'):
            self.change_contract(phase, 'pending', required=False)
        self.add_case('failed', 'failed')
        self.assertTrue(compare(self.metadata, self.results)['ready'])
        self.change_contract('candidate', 'newly_passing', required=False)
        self.assertFalse(compare(self.metadata, self.results)['ready'])

    def test_crash_missing_replay_or_nonreproducible_failure_never_passes(self):
        self.change_contract('candidate', 'failed')
        for before, after in (('passed', 'passed'), ('passed', 'error'), ('inconclusive', 'failed')):
            self.add_case(before, after)
            self.assertFalse(compare(self.metadata, self.results)['ready'])
        write_json(
            self.results / f'results-campaign-candidate-{self.shard}/campaign.json',
            {'status': 'failed', 'exit_code': -11},
        )
        self.assertFalse(compare(self.metadata, self.results)['ready'])

    def test_partial_campaign_retains_original_and_deduplicates_concrete_variants(self):
        self.add_case('passed', 'failed')
        (self.results / f'results-campaign-candidate-{self.shard}/campaign.json').unlink()
        self.assertEqual(len(discover_cases(self.results)), 1)
        changed = copy.deepcopy(self.saved)
        changed.update(revision='new', source_sha256='new', versions={'node': '27'})
        changed['settings']['seed'] = 99
        self.assertEqual(case_id(changed), case_id(self.saved))
        changed['metamorphic'][0]['source'] += '\n// different concrete form'
        self.assertNotEqual(case_id(changed), case_id(self.saved))
        import dependency_evidence

        previous = dependency_evidence.EVIDENCE
        self.addCleanup(setattr, dependency_evidence, 'EVIDENCE', previous)
        dependency_evidence.EVIDENCE = self.results / 'retained'
        comparison = compare(self.metadata, self.results)
        self.assertEqual(len(retain_evidence(self.metadata, comparison, self.results)), 1)
        self.assertEqual(retain_evidence(self.metadata, comparison, self.results), [])

    def test_pins_advance_together_without_changing_other_constraints(self):
        source = (ROOT / 'Cargo.toml').read_text()
        updated = manifest_with_revisions(source, {'perry': 'a' * 40, 'pliron': 'b' * 40})
        import tomllib

        old = tomllib.loads(source)
        new = tomllib.loads(updated)
        for name, revision in (('perry-hir', 'a' * 40), ('perry-parser', 'a' * 40), ('pliron', 'b' * 40)):
            self.assertEqual(new['workspace']['dependencies'][name]['rev'], revision)
            new['workspace']['dependencies'][name]['rev'] = old['workspace']['dependencies'][name]['rev']
        self.assertEqual(new, old)
        with self.assertRaises(ValueError):
            manifest_with_revisions(source, {'perry': 'not-a-revision'})

    def test_command_deadline_preserves_a_nonpassing_result(self):
        result = run_logged([sys.executable, '-c', 'import time; time.sleep(30)'], self.results / 'timeout.log', 0.05)
        self.assertEqual(result['status'], 'timed_out')
        self.assertLess(result['seconds'], 6)


class BranchTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.remote = self.root / 'remote.git'
        self.work = self.root / 'work'
        self.environment = dict(os.environ, GIT_CONFIG_GLOBAL=os.devnull, GH_TOKEN='test-token')
        self.git('init', '--bare', str(self.remote), cwd=self.root)
        self.git('init', '-b', 'main', str(self.work), cwd=self.root)
        self.git('config', 'user.name', 'Test')
        self.git('config', 'user.email', 'test@example.test')
        (self.work / '.gitignore').write_text('artifacts/\n')
        (self.work / 'base').write_text('base\n')
        self.git('add', '.')
        self.git('commit', '-m', 'base')
        self.git('remote', 'add', 'origin', str(self.remote))
        self.git('push', 'origin', 'main')
        self.git('checkout', '-b', 'codex/update-dependencies')
        (self.work / 'human-fix').write_text('keep this fix\n')
        self.git('add', '.')
        self.git('commit', '-m', 'human fix')
        self.human = self.git('rev-parse', 'HEAD')
        self.git('push', 'origin', 'HEAD')
        self.git('checkout', 'main')

    def git(self, *args, cwd=None):
        return subprocess.check_output(
            ['git', *args], cwd=cwd or self.work, env=self.environment, text=True, stderr=subprocess.DEVNULL
        ).strip()

    def branch(self, *args):
        return subprocess.run(
            ['bash', str(BRANCH_SCRIPT), *args],
            cwd=self.work,
            env=self.environment,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )

    def publication(self):
        state = json.loads((self.work / 'artifacts/source/branch.json').read_text())
        write_json(self.work / 'artifacts/source/metadata.json', state)
        write_json(self.work / 'artifacts/comparison/comparison.json', {'ready': True})
        (self.work / 'artifacts/comparison/summary.md').write_text('test summary')
        return state

    def test_refresh_preserves_human_commits_and_does_not_create_empty_commits(self):
        (self.work / 'new-main').write_text('new main\n')
        self.git('add', '.')
        self.git('commit', '-m', 'main advance')
        self.git('push', 'origin', 'main')
        result = self.branch('prepare')
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertTrue((self.work / 'human-fix').exists())
        self.assertTrue((self.work / 'new-main').exists())
        self.git('merge-base', '--is-ancestor', self.human, 'HEAD')
        before = self.git('rev-parse', 'HEAD')
        self.assertEqual(self.branch('commit', 'no change', 'base').returncode, 0)
        self.assertEqual(self.git('rev-parse', 'HEAD'), before)

    def test_concurrent_human_push_and_non_descendant_are_never_overwritten(self):
        self.assertEqual(self.branch('prepare').returncode, 0)
        state = self.publication()
        (self.work / 'human-fix').write_text('newer fix\n')
        self.git('add', '.')
        self.git('commit', '-m', 'concurrent fix')
        self.git('push', 'origin', 'HEAD')
        newest = self.git('rev-parse', 'HEAD')
        result = self.branch('publish')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('changed during validation', result.stdout)
        state['remote_head'] = newest
        write_json(self.work / 'artifacts/source/metadata.json', state)
        self.git('checkout', '--detach', 'main')
        self.assertNotEqual(self.branch('publish').returncode, 0)
        self.assertEqual(self.git('ls-remote', 'origin', 'refs/heads/codex/update-dependencies').split()[0], newest)

    def test_merge_conflict_leaves_original_branch_intact(self):
        self.git('checkout', 'codex/update-dependencies')
        (self.work / 'base').write_text('branch change\n')
        self.git('commit', '-am', 'branch conflict')
        self.git('push', 'origin', 'HEAD')
        before = self.git('rev-parse', 'HEAD')
        self.git('checkout', 'main')
        (self.work / 'base').write_text('main change\n')
        self.git('commit', '-am', 'main conflict')
        self.git('push', 'origin', 'main')
        self.assertNotEqual(self.branch('prepare').returncode, 0)
        self.assertEqual(self.git('rev-parse', 'HEAD'), before)
        self.assertEqual(self.git('status', '--porcelain'), '')

    def test_bundle_restores_exact_baseline_and_candidate(self):
        self.assertEqual(self.branch('prepare').returncode, 0)
        state = self.publication()
        (self.work / 'base').write_text('candidate\n')
        self.git('commit', '-am', 'dependency update')
        state['candidate'] = self.git('rev-parse', 'HEAD')
        write_json(self.work / 'artifacts/source/metadata.json', state)
        self.assertEqual(self.branch('bundle').returncode, 0)
        for phase in ('baseline', 'candidate'):
            result = self.branch('restore', phase)
            self.assertEqual(result.returncode, 0, result.stdout)
            self.assertEqual(self.git('rev-parse', 'HEAD'), state[phase])

    def test_existing_pr_changes_draft_state_without_creating_a_duplicate(self):
        self.assertEqual(self.branch('prepare').returncode, 0)
        self.publication()
        tools = self.root / 'bin'
        tools.mkdir()
        log = self.root / 'gh.log'
        gh = tools / 'gh'
        gh.write_text(
            f'#!{sys.executable}\n'
            + 'import json, os, pathlib, sys\n'
            + f"with pathlib.Path({str(log)!r}).open('a') as log: log.write(json.dumps(sys.argv[1:]) + '\\n')\n"
            + "if sys.argv[1:3] == ['pr', 'list']: print(json.dumps({'number': 7, 'isDraft': os.environ['TEST_DRAFT'] == 'true'}))\n"
        )
        gh.chmod(0o755)
        self.environment['PATH'] = str(tools) + os.pathsep + os.environ['PATH']
        for ready, draft in ((True, True), (False, False), (True, False)):
            self.environment['TEST_DRAFT'] = str(draft).lower()
            write_json(self.work / 'artifacts/comparison/comparison.json', {'ready': ready})
            log.write_text('')
            result = self.branch('publish')
            self.assertEqual(result.returncode, int(not ready), result.stdout)
            calls = [json.loads(line) for line in log.read_text().splitlines()]
            self.assertFalse(any(call[:2] == ['pr', 'create'] for call in calls))
            transitions = [call for call in calls if call[:2] == ['pr', 'ready']]
            if ready == draft:
                self.assertEqual(transitions, [['pr', 'ready', '7'] + ([] if ready else ['--undo'])])
            else:
                self.assertEqual(transitions, [])


if __name__ == '__main__':
    unittest.main()
