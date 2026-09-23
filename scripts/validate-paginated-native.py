"""Validate in-place edits with a matching native binary, using synthetic data only.

No turn/start, tool calls, authentication copying, or existing Codex homes are used.
The output directory must not exist. This tests app-server restarts, not Desktop UI
cold startup. Run after compiling the Rust test target to shorten each edit step.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import queue
import subprocess
import threading
import uuid

REPO = Path(__file__).resolve().parents[1]


class Native:
    def __init__(self, binary, home):
        self.sequence = 0
        self.messages = queue.Queue()
        self.log = (home / 'native-stderr.log').open('a', encoding='utf-8')
        self.process = subprocess.Popen(
            [str(binary), 'app-server', '--stdio'], cwd=home,
            env={**os.environ, 'CODEX_HOME': str(home)},
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.log,
            text=True, encoding='utf-8',
            creationflags=getattr(subprocess, 'CREATE_NO_WINDOW', 0),
        )

        def read():
            for line in self.process.stdout:
                self.messages.put(json.loads(line))
        threading.Thread(target=read, daemon=True).start()
        self.call('initialize', {'clientInfo': {'name': 'cc_sessions_fixture', 'version': '1'},
                                 'capabilities': {'experimentalApi': True}})
        self.process.stdin.write('{"method":"initialized"}\n')
        self.process.stdin.flush()

    def call(self, method, params):
        self.sequence += 1
        self.process.stdin.write(json.dumps({'id': self.sequence, 'method': method, 'params': params}) + '\n')
        self.process.stdin.flush()
        while True:
            response = self.messages.get(timeout=40)
            if response.get('id') == self.sequence:
                if 'error' in response:
                    raise AssertionError((method, response))
                return response['result']

    def close(self):
        self.process.stdin.close()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.terminate()  # only the child created above
            self.process.wait(timeout=5)
        self.log.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--codex', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--with-tools', action='store_true', help='Include a command chain, failed turn and interrupted turn')
    args = parser.parse_args()
    binary = args.codex.resolve()
    version = subprocess.check_output([str(binary), '--version'], text=True).strip()
    assert version == 'codex-cli 0.155.0-alpha.16', f'Unverified native version: {version}'
    home = args.output.resolve()
    home.mkdir(parents=True, exist_ok=False)
    native = Native(binary, home)
    try:
        started = native.call('thread/start', {'cwd': str(home), 'historyMode': 'paginated',
                                             'modelProvider': 'openai', 'ephemeral': False})
    finally:
        native.close()
    tid = started['thread']['id']
    path = Path(started['thread']['path']).resolve()
    assert path.is_relative_to(home)
    path.parent.mkdir(parents=True, exist_ok=True)
    turns = [str(uuid.uuid4()) for _ in range(3)]
    rows = [{'type': 'session_meta', 'payload': {
        'id': tid, 'session_id': tid, 'timestamp': '2026-09-23T10:00:00Z',
        'cwd': str(home), 'runtime_workspace_roots': [str(home)],
        'originator': 'cc_sessions_fixture', 'cli_version': '0.155.0-alpha.16',
        'source': 'cli', 'model_provider': 'openai', 'history_mode': 'paginated',
        'base_instructions': {'text': 'Synthetic edit regression. Do not execute tools.'},
    }}]
    for k, text in enumerate(['KEEP-A', 'DELETE-B', 'KEEP-C']):
        turn = turns[k]
        rows.extend([
            {'type': 'event_msg', 'payload': {'type': 'task_started', 'turn_id': turn,
                'root_turn_id': turn, 'started_at': 1790150000,
                'model_context_window': 258400, 'collaboration_mode_kind': 'default'}},
            {'type': 'response_item', 'payload': {'type': 'message', 'id': f'context-{k}',
                'role': 'user', 'content': [{'type': 'input_text', 'text': text}],
                'internal_chat_message_metadata_passthrough': {'turn_id': turn,
                    'create_time': 1790150000, 'content_item_kinds': ['user.text']}}},
            {'type': 'event_msg', 'payload': {'type': 'item_completed', 'thread_id': tid,
                'turn_id': turn, 'started_at_ms': 1790150000000, 'completed_at_ms': 1790150000000,
                'item': {'type': 'UserMessage', 'id': f'user-{k}', 'client_id': None,
                    'content': [{'type': 'text', 'text': text, 'text_elements': []}]}}},
            {'type': 'event_msg', 'payload': {'type': 'item_completed', 'thread_id': tid,
                'turn_id': turn, 'started_at_ms': 1790150001000, 'completed_at_ms': 1790150001000,
                'item': {'type': 'AgentMessage', 'id': f'agent-{k}', 'phase': 'final_answer',
                    'content': [{'type': 'Text', 'text': f'reply-{k}'}]}}},
            {'type': 'response_item', 'payload': {'type': 'message', 'id': f'agent-{k}',
                'role': 'assistant', 'phase': 'final_answer',
                'content': [{'type': 'output_text', 'text': f'reply-{k}'}]}},
            {'type': 'event_msg', 'payload': {'type': 'task_complete', 'turn_id': turn,
                'last_agent_message': f'reply-{k}', 'started_at': 1790150000,
                'completed_at': 1790150001, 'duration_ms': 1000}},
        ])
    if args.with_tools:
        rows[12]['payload']['error'] = {'message': 'synthetic failure'}
        rows[18]['payload']['type'] = 'turn_aborted'
        rows[18]['payload']['reason'] = 'interrupted'
        rows[18]['payload'].pop('last_agent_message')
        rows[10:10] = [
            {'type': 'response_item', 'payload': {'type': 'function_call', 'name': 'exec_command',
                'call_id': 'call-1', 'arguments': '{"cmd":"synthetic-never-execute"}'}},
            {'type': 'event_msg', 'payload': {'type': 'item_completed', 'thread_id': tid,
                'turn_id': turns[1], 'started_at_ms': 1790150000000, 'completed_at_ms': 1790150001000,
                'item': {'type': 'CommandExecution', 'id': 'tool-1', 'command': ['synthetic-never-execute'],
                    'cwd': home.as_uri(), 'parsed_cmd': [], 'source': 'agent', 'status': 'completed',
                    'aggregated_output': 'synthetic output', 'exit_code': 0}}},
            {'type': 'response_item', 'payload': {'type': 'function_call_output', 'call_id': 'call-1',
                'output': 'synthetic output'}},
        ]
    for i, row in enumerate(rows):
        row.update(ordinal=i, timestamp='2026-09-23T10:00:00Z')
    path.write_text(''.join(json.dumps(row) + '\n' for row in rows), encoding='utf-8', newline='\n')
    (home / '.cc-synthetic-fixture.json').write_text(json.dumps({
        'id': tid, 'path': str(path), 'turns': turns,
        'range_items': ['user-1', 'tool-1', 'agent-1'] if args.with_tools else ['user-1', 'agent-1']}), encoding='utf-8')

    def edit(action):
        result = subprocess.run(['cargo', 'test', '--manifest-path', 'src-tauri/Cargo.toml',
            '--lib', 'paginated_native_fixture_command', '--', '--ignored', '--nocapture'],
            cwd=REPO, env={**os.environ, 'CC_SYNTHETIC_HOME': str(home), 'CC_SYNTHETIC_ACTION': action},
            capture_output=True, text=True, encoding='utf-8')
        with (home / 'rust-commands.log').open('a', encoding='utf-8') as log:
            log.write(action + '\n' + result.stdout + result.stderr)
        assert result.returncode == 0, result.stdout + result.stderr

    native = Native(binary, home)
    try:
        initial = native.call('thread/resume', {'threadId': tid, 'excludeTurns': True})
        edit('busy')  # prove actual native ownership, not just an advisory check
    finally:
        native.close()
    original = path.read_bytes()
    stages = []
    all_ids = ['user-0', 'agent-0', 'user-1', 'agent-1', 'user-2', 'agent-2']
    if args.with_tools:
        all_ids.insert(3, 'tool-1')

    def read_stage(stage, expected_ids, preview='KEEP-A', texts=None, resume=False):
        before = hashlib.sha256(path.read_bytes()).hexdigest()
        native = Native(binary, home)
        evidence = {'version': version, 'threadId': tid, 'stage': stage}
        try:
            if resume:
                evidence['resume'] = native.call('thread/resume', {'threadId': tid, 'excludeTurns': True})
                assert evidence['resume']['thread']['id'] == tid
            evidence['thread'] = native.call('thread/read', {'threadId': tid, 'includeTurns': False})['thread']
            assert evidence['thread']['id'] == tid
            assert evidence['thread']['preview'] == preview
            for method in ['thread/items/list', 'thread/turns/list']:
                data, cursor = [], None
                while True:
                    params = {'threadId': tid, 'limit': 2}
                    if cursor:
                        params['cursor'] = cursor
                    page = native.call(method, params)
                    data.extend(page['data'])
                    cursor = page.get('nextCursor')
                    if not cursor:
                        break
                evidence[method] = data
            items = [v['item'] for v in evidence['thread/items/list']]
            assert [v['id'] for v in items] == expected_ids
            if args.with_tools:
                states = {turn['id']: turn['status'] for turn in evidence['thread/turns/list']}
                assert states[turns[1]] == 'failed', states
                assert states[turns[2]] == 'interrupted', states
            for item_id, text in (texts or {}).items():
                item = next(v for v in items if v['id'] == item_id)
                actual = item.get('text', ''.join(c.get('text', '') for c in item.get('content', [])))
                assert actual == text, (item, text)
            if 'user-1' not in expected_ids:
                assert 'DELETE-B' not in json.dumps(evidence)
                assert 'DELETE-B' not in path.read_text(encoding='utf-8')
            if 'agent-1' not in expected_ids:
                assert 'reply-1' not in json.dumps(evidence)
                assert 'reply-1' not in path.read_text(encoding='utf-8')
        finally:
            native.close()
        evidence['rolloutSha256BeforeRead'] = before
        evidence['rolloutSha256AfterRead'] = hashlib.sha256(path.read_bytes()).hexdigest()
        if not resume:
            assert evidence['rolloutSha256AfterRead'] == before
        (home / (stage + '.json')).write_text(json.dumps(evidence, indent=2), encoding='utf-8')
        stages.append(stage)
        print(stage + ': passed', flush=True)

    read_stage('baseline', all_ids)
    edit('delete'); read_stage('single-delete', [i for i in all_ids if i != 'user-1'])
    edit('undo'); read_stage('undo-delete', all_ids)
    assert path.read_bytes() == original
    edit('undo'); read_stage('redo-delete', [i for i in all_ids if i != 'user-1'])
    edit('undo'); read_stage('undo-redo', all_ids)
    edit('range'); read_stage('range-delete', [i for i in all_ids if i not in ('user-1', 'agent-1', 'tool-1')])
    edit('undo'); read_stage('undo-range', all_ids)
    edit('rewrite'); read_stage('rewrite-user', all_ids, 'NATIVE-EDITED-A', {'user-0': 'NATIVE-EDITED-A'})
    edit('rewrite-assistant'); read_stage('rewrite-assistant', all_ids, 'NATIVE-EDITED-A', {'agent-0': 'NATIVE-EDITED-ASSISTANT'})
    edit('restore'); read_stage('restore-snapshot', all_ids)
    assert path.read_bytes() == original
    edit('delete'); read_stage('resume-edited-history', [i for i in all_ids if i != 'user-1'], resume=True)
    (home / 'result.json').write_text(json.dumps({
        'passed': True, 'version': version, 'threadId': tid, 'stages': stages,
        'nativeWriterRejection': True, 'nativeProcessRestarts': True,
        'desktopColdStartup': 'NOT VERIFIED', 'modelGeneration': False, 'toolReplay': False,
        'toolsAndTerminalStates': args.with_tools,
    }, indent=2), encoding='utf-8')
    print(str(home / 'result.json'), flush=True)


if __name__ == '__main__':
    main()
