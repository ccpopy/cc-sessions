"""Native revert/rollout identity regression using only a new isolated home.

Uses alpha.16 thread/revert and a loopback model fixture; never copies credentials,
opens a user data root, or executes historical tools. Reports contain identities,
counts and hashes. Desktop GUI cold startup is a separate acceptance step.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import threading
import urllib.request
from http.server import ThreadingHTTPServer

REPO = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('media', REPO/'scripts/validate-paginated-media.py')
media = importlib.util.module_from_spec(spec)
spec.loader.exec_module(media)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--codex', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    binary = args.codex.resolve()
    version = subprocess.check_output([str(binary), '--version'], text=True).strip()
    assert version == 'codex-cli 0.155.0-alpha.16', version
    home = args.output.resolve()
    home.mkdir(parents=True, exist_ok=False)
    catalog = urllib.request.urlopen(
        f'https://raw.githubusercontent.com/openai/codex/{media.SOURCE_COMMIT}/codex-rs/models-manager/models.json', timeout=30).read()
    (home/'models.json').write_bytes(catalog)
    server = ThreadingHTTPServer(('127.0.0.1', 0), media.ResponseHandler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    (home/'config.toml').write_text(
        'model="gpt-5.5"\nmodel_provider="fixture"\napproval_policy="never"\nsandbox_mode="read-only"\n'
        'model_catalog_json='+json.dumps(str(home/'models.json'))+'\n'
        '[model_providers.fixture]\nname="Loopback fixture"\nwire_api="responses"\n'
        f'base_url="http://127.0.0.1:{server.server_port}/v1"\n'
        'requires_openai_auth=false\nsupports_websockets=false\nrequest_max_retries=0\nstream_max_retries=0\n',
        encoding='utf-8')
    native = media.NativeCapture(binary, home)
    try:
        started = native.call('thread/start', {'cwd': str(home), 'historyMode': 'paginated',
            'modelProvider': 'fixture', 'model': 'gpt-5.5', 'ephemeral': False,
            'baseInstructions': 'Synthetic fixture. Do not execute tools.'})
        tid = started['thread']['id']
        original_path = Path(started['thread']['path']).resolve()
        turns = []
        for prompt in ['INHERITED-A', 'REVERT-THIS']:
            turn = native.call('turn/start', {'threadId': tid, 'input': [
                {'type': 'text', 'text': prompt, 'text_elements': []}]})['turn']['id']
            assert native.wait_turn(turn)['status'] == 'completed'
            turns.append(turn)
        reverted = native.call('thread/revert', {'threadId': tid, 'beforeTurnId': turns[-1]})
        assert reverted['thread']['id'] == tid
        path = Path(reverted['thread']['path']).resolve()
        assert path != original_path and path.is_relative_to(home)
        second_turns = []
        for prompt in ['KEEP-A', 'REVERT-AGAIN']:
            turn = native.call('turn/start', {'threadId': tid, 'input': [
                {'type': 'text', 'text': prompt, 'text_elements': []}]})['turn']['id']
            assert native.wait_turn(turn)['status'] == 'completed'
            second_turns.append(turn)
        second = native.call('thread/revert', {'threadId': tid, 'beforeTurnId': second_turns[-1]})
        switched_parent = path
        path = Path(second['thread']['path']).resolve()
        assert path != switched_parent and path.is_relative_to(home)
        for prompt in ['DELETE-B', 'KEEP-C']:
            turn = native.call('turn/start', {'threadId': tid, 'input': [
                {'type': 'text', 'text': prompt, 'text_elements': []}]})['turn']['id']
            assert native.wait_turn(turn)['status'] == 'completed'
    finally:
        native.close()
        server.shutdown()
    original_prefix = original_path.read_bytes()
    switched_prefix = switched_parent.read_bytes()
    original = path.read_bytes()
    rid = path.stem.rsplit('_', 1)[1]
    (home/'capture.json').write_text(json.dumps({'threadId': tid, 'path': str(path),
        'nativeGenerated': True, 'version': version}), encoding='utf-8')
    built = subprocess.run(['cargo', 'test', '--manifest-path', 'src-tauri/Cargo.toml',
        '--lib', '--no-run', '--message-format=json'], cwd=REPO,
        capture_output=True, text=True, encoding='utf-8', check=True)
    binaries = [json.loads(line)['executable'] for line in built.stdout.splitlines()
        if line.startswith('{') and json.loads(line).get('reason') == 'compiler-artifact'
        and json.loads(line).get('executable')]
    assert len(binaries) == 1
    evidence = []

    def read(stage):
        client = media.module.Native(binary, home)
        before = path.read_bytes()
        try:
            thread = client.call('thread/read', {'threadId': tid, 'includeTurns': False})['thread']
            assert thread['id'] == tid and Path(thread['path']).resolve() == path
            data, cursor = [], None
            while True:
                params = {'threadId': tid, 'limit': 2}
                if cursor:
                    params['cursor'] = cursor
                page = client.call('thread/items/list', params)
                data.extend(page['data'])
                cursor = page.get('nextCursor')
                if not cursor:
                    break
        finally:
            client.close()
        assert path.read_bytes() == before and original_path.read_bytes() == original_prefix
        assert switched_parent.read_bytes() == switched_prefix
        items = [row['item'] for row in data]
        evidence.append({'stage': stage, 'itemIds': [item['id'] for item in items],
            'rolloutSha256': hashlib.sha256(before).hexdigest(), 'nativeProcessRestart': True})
        return items

    def edit(action, target=''):
        subprocess.run([binaries[0], 'edit::paginated::tests::paginated_native_media_fixture_command',
            '--exact', '--ignored'], cwd=REPO, env={**os.environ, 'CC_NATIVE_MEDIA_HOME': str(home),
            'CC_NATIVE_MEDIA_ACTION': action, 'CC_NATIVE_MEDIA_ITEM': target,
            'CC_NATIVE_MEDIA_BLOCK': '0', 'CC_NATIVE_MEDIA_TEXT': 'EDITED-B'},
            capture_output=True, check=True)
        result = json.loads((home/'last-operation.json').read_text(encoding='utf-8'))
        assert result['ok'], result

    baseline = read('baseline')
    (home/'history-read-contract.json').write_text(json.dumps({'items': baseline}), encoding='utf-8')
    subprocess.run([binaries[0], 'convert::tests::native_isolated_logical_read_contract', '--exact', '--ignored'],
        cwd=REPO, env={**os.environ, 'CC_NATIVE_READ_HOME': str(home)}, check=True)

    target = next(item['id'] for item in baseline if item.get('content', [{}])[0].get('text') == 'DELETE-B')
    edit('edit', target)
    edited = read('edited')
    assert next(item for item in edited if item['id'] == target)['content'][0]['text'] == 'EDITED-B'
    assert [item for item in edited if item['id'] != target] == [item for item in baseline if item['id'] != target]
    edit('undo')
    assert read('undo-edit') == baseline and path.read_bytes() == original
    edit('delete', target)
    assert read('deleted') == [item for item in baseline if item['id'] != target]
    edit('undo')
    assert read('undo-delete') == baseline and path.read_bytes() == original
    report = {'passed': True, 'version': version, 'sourceCommit': media.SOURCE_COMMIT,
        'threadId': tid, 'rolloutId': rid, 'targetItemId': target, 'stages': evidence,
        'inheritedRolloutUnchanged': True, 'undoByteExact': True,
        'desktopColdStartup': 'NOT VERIFIED', 'toolReplay': False}
    (home/'result.json').write_text(json.dumps(report, indent=2), encoding='utf-8')
    print(json.dumps({'passed': True, 'threadId': tid, 'rolloutId': rid, 'stages': len(evidence)}))


if __name__ == '__main__':
    main()
