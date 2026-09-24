"""Run the pinned native contract without touching any existing client home.

Only the structural summary is suitable for CI artifact upload. Detailed evidence,
logs and synthetic homes remain under the ignored output directory.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

REPO = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--codex', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    binary = args.codex.resolve()
    profile = json.loads((REPO / 'scripts/native-contract-profile.json').read_text(encoding='utf-8'))
    version = subprocess.check_output([str(binary), '--version'], text=True).strip()
    if version != 'codex-cli ' + profile['codex']['version']:
        raise RuntimeError('Native version does not match the pinned contract')
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    report = {
        'schema_version': 1, 'source_sha': subprocess.check_output(
            ['git', 'rev-parse', 'HEAD'], cwd=REPO, text=True).strip(),
        'working_tree_dirty': bool(subprocess.check_output(['git', 'status', '--porcelain'], cwd=REPO, text=True).strip()),
        'profile': profile, 'version': version,
        'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
        'scenarios': [], 'passed': False,
    }
    scenarios = [
        ('edit-tools-blocks', 'validate-paginated-native.py', ['--with-tools', '--with-blocks']),
        ('native-media', 'validate-paginated-media.py', []),
        ('native-async-questions', 'validate-paginated-media.py', ['--async-questions']),
        ('native-rollout-switch', 'validate-paginated-projection.py', []),
    ]
    try:
        for name, script, flags in scenarios:
            with (output / (name + '.log')).open('w', encoding='utf-8') as log:
                result = subprocess.run([sys.executable, '-B', str(REPO / 'scripts' / script),
                    '--codex', str(binary), '--output', str(output / name), *flags],
                    cwd=REPO, stdout=log, stderr=subprocess.STDOUT,
                    env={**os.environ, 'PYTHONUTF8': '1'}, timeout=1800)
            evidence = output / name / 'result.json'
            passed = result.returncode == 0 and evidence.is_file() and json.loads(evidence.read_text(encoding='utf-8')).get('passed') is True
            report['scenarios'].append({'name': name, 'passed': passed, 'exit_code': result.returncode,
                'evidence_sha256': hashlib.sha256(evidence.read_bytes()).hexdigest() if evidence.is_file() else None})
            print(f'{name}: {"passed" if passed else "FAILED"}', flush=True)
        report['passed'] = all(s['passed'] for s in report['scenarios'])
    finally:
        (output / 'contract-summary.json').write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding='utf-8')
    if not report['passed']:
        raise SystemExit(1)


if __name__ == '__main__':
    main()
