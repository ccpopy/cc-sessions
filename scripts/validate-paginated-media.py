"""Capture native media history, then verify edits and cold native reads in isolation.

The matching CLI writes every original record through thread/start and turn/start.
Only a loopback Responses fixture is contacted; no credentials are copied and no
real model or external tool runs. The output directory must not already exist. Desktop GUI
acceptance is separate from the native process restarts performed here.
"""
import argparse
import base64
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import threading
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import wave

REPO = Path(__file__).resolve().parents[1]
SOURCE_COMMIT = '0e2f848bf4a4e8d41a02d848a851ba126c09d185'
SOURCE_COMMITS = {
    '0.153.4': '3d2ee51ca2d5db578f328aa75e20aa22c0197c9a',
    '0.155.0-alpha.9.2': '4607249e430dac1c961df4dc615beae88e33cec8',
    '0.155.0-alpha.16': SOURCE_COMMIT,
}
spec = importlib.util.spec_from_file_location('native_validator', REPO/'scripts/validate-paginated-native.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class NativeCapture(module.Native):
    def __init__(self, *args):
        self.notifications=[]
        super().__init__(*args)

    def call(self, method, params):
        self.sequence += 1
        self.process.stdin.write(json.dumps({'id':self.sequence,'method':method,'params':params})+'\n')
        self.process.stdin.flush()
        while True:
            message=self.messages.get(timeout=40)
            if message.get('id')==self.sequence:
                assert 'error' not in message,(method,message)
                return message['result']
            self.notifications.append(message)

    def wait_turn(self, turn):
        while True:
            for i,message in enumerate(self.notifications):
                if message.get('method')=='turn/completed' and message['params']['turn']['id']==turn:
                    self.notifications.pop(i)
                    return message['params']['turn']
            self.notifications.append(self.messages.get(timeout=40))


class ResponseHandler(BaseHTTPRequestHandler):
    count = 0
    async_questions = False

    def log_message(self, *args):
        pass

    def do_POST(self):
        if not self.path.endswith('/responses'):
            self.send_error(404)
            return
        self.rfile.read(int(self.headers.get('Content-Length', '0')))
        type(self).count += 1
        n = type(self).count
        events = [
            {'type': 'response.created', 'response': {'id': f'resp-{n}'}},
            {'type': 'response.output_item.done', 'item': {'type':'message','role':'assistant',
             'id':f'fixture-answer-{n}','content':[{'type':'output_text','text':f'fixture-reply-{n}'}]}},
            {'type': 'response.completed', 'response': {'id': f'resp-{n}', 'usage':
             {'input_tokens':0,'input_tokens_details':None,'output_tokens':0,'output_tokens_details':None,'total_tokens':0}}},
        ]
        if type(self).async_questions and n % 2 == 1:
            case = (n - 1) // 2
            questions = [
                [{'title':'SYNTHETIC QUESTION'}],
                [{'title':'SYNTHETIC CHOICE','options':['FIRST','SECOND']}],
                [{'title':'FIRST QUESTION','options':['ONE','TWO']},{'title':'SECOND QUESTION'}],
            ][case]
            events[1]['item'] = {'type':'function_call','id':f'fc-native-{case}','call_id':f'call-native-{case}',
                'namespace':'functions','name':'request_user_input_async','arguments':json.dumps({'questions':questions})}
        body = ''.join(f'event: {e["type"]}\ndata: {json.dumps(e)}\n\n' for e in events).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'text/event-stream')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--codex', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--capture-only', action='store_true', help='Keep an untouched native baseline without running CC Sessions')
    parser.add_argument('--async-questions', action='store_true', help='Exercise native request_user_input_async messages and their call chains')
    args = parser.parse_args()
    version = subprocess.check_output([str(args.codex), '--version'], text=True).strip()
    source_commit = SOURCE_COMMITS[version.removeprefix('codex-cli ')]
    home = args.output.resolve()
    home.mkdir(parents=True, exist_ok=False)
    ResponseHandler.count = 0
    ResponseHandler.async_questions = args.async_questions
    server = ThreadingHTTPServer(('127.0.0.1',0),ResponseHandler)
    threading.Thread(target=server.serve_forever,daemon=True).start()
    catalog_bytes = urllib.request.urlopen(f'https://raw.githubusercontent.com/openai/codex/{source_commit}/codex-rs/models-manager/models.json', timeout=30).read()
    catalog = json.loads(catalog_bytes)
    selected = next(m for m in catalog['models'] if m['slug']=='gpt-5.5')
    selected['input_modalities'] = ['text','image','audio']
    if args.async_questions:
        selected['tool_mode'] = 'code_mode_only'
        selected['experimental_supported_tools'] = ['request_user_input_async']
    (home/'models.json').write_text(json.dumps({'models':[selected]}),encoding='utf-8')
    (home/'config.toml').write_text(
        'model = "gpt-5.5"\nmodel_provider = "fixture"\napproval_policy = "never"\n'
        'sandbox_mode = "read-only"\nmodel_catalog_json = '+json.dumps(str(home/'models.json'))+'\n'
        '[model_providers.fixture]\nname = "Local fixture"\nwire_api = "responses"\n'
        f'base_url = "http://127.0.0.1:{server.server_port}/v1"\n'
        'requires_openai_auth = false\nsupports_websockets = false\nrequest_max_retries = 0\nstream_max_retries = 0\n',
        encoding='utf-8')
    png='iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg=='
    for name in ['one.png','two.png']:
        (home/name).write_bytes(base64.b64decode(png))
    with wave.open(str(home/'tone.wav'),'wb') as f:
        f.setnchannels(1); f.setsampwidth(2); f.setframerate(8000); f.writeframes(b'\0\0'*800)
    text=lambda t:{'type':'text','text':t,'text_elements':[]}
    image=lambda n:{'type':'localImage','path':str(home/n)}
    audio={'type':'localAudio','path':str(home/'tone.wav')}
    cases = [
        ('pure-image',[image('one.png')]),
        ('text-image',[text('KEEP-A'),image('one.png')]),
        ('multi-image',[image('one.png'),text('BETWEEN'),image('two.png'),text('TAIL')]),
        ('local-audio',[text('LISTEN'),audio,text('AUDIO-TAIL')]),
        ('mixed-blocks',[text('HEAD'),text('SECOND'),image('one.png'),text('MIDDLE'),audio,text('LAST')]),
        ('inline-image',[text('INLINE'),{'type':'image','url':'data:image/png;base64,'+png},text('INLINE-TAIL')]),
        ('inline-audio',[text('INLINE-AUDIO'),{'type':'audio','url':'data:audio/wav;base64,'+base64.b64encode((home/'tone.wav').read_bytes()).decode()},text('AUDIO-END')]),
        ('literal-tags',[text('<image>'),text('  KEEP WHITESPACE  '),text('</audio>')]),
    ]
    if args.async_questions:
        cases = [(name,[text('FIXTURE '+name)]) for name in ['freeform','options','multi-question']]
    native = NativeCapture(args.codex.resolve(),home)
    try:
        started=native.call('thread/start',{'cwd':str(home),'historyMode':'paginated','model':'gpt-5.5',
            'modelProvider':'fixture','ephemeral':False,'baseInstructions':'Fixture only. Never execute tools.'})
        tid=started['thread']['id']
        path=Path(started['thread']['path']).resolve()
        assert path.is_relative_to(home)
        captured=[]
        for name,inputs in cases:
            turn=native.call('turn/start',{'threadId':tid,'input':inputs})['turn']['id']
            state=native.wait_turn(turn)
            assert state['status']=='completed', (name,state['status'],state.get('error'))
            captured.append({'case':name,'turnId':turn})
            print(name,turn,flush=True)
        (home/'native-initial-items.json').write_text(json.dumps(native.call('thread/items/list',{'threadId':tid,'limit':100}),indent=2),encoding='utf-8')
    finally:
        native.close()
        server.shutdown()
    original=path.read_bytes()
    (home/'untouched-native.jsonl').write_bytes(original)
    (home/'capture.json').write_text(json.dumps({'version':version,'sourceCommit':source_commit,'caseSet':'async' if args.async_questions else 'media',
        'threadId':tid,'path':str(path),'cases':captured,'rolloutSha256':hashlib.sha256(original).hexdigest(),
        'nativeGenerated':True,'ccSessionsEdits':False,'modelService':'loopback fixture','requests':ResponseHandler.count,
        'catalogSha256':hashlib.sha256(catalog_bytes).hexdigest(),'fixtureModalities':['text','image','audio']},indent=2),encoding='utf-8')
    print('captured',tid,len(original),flush=True)
    if not args.capture_only:
        verify(args.codex.resolve(),home,tid,path,captured)


def verify(binary, home, tid, path, cases):
    capture=json.loads((home/'capture.json').read_text(encoding='utf-8'))
    async_questions=capture.get('caseSet')=='async'
    built = subprocess.run(['cargo','test','--manifest-path','src-tauri/Cargo.toml','--lib','--no-run','--message-format=json'],
        cwd=REPO,capture_output=True,text=True,encoding='utf-8',check=True)
    executables=[json.loads(line)['executable'] for line in built.stdout.splitlines()
                 if line.startswith('{') and json.loads(line).get('reason')=='compiler-artifact' and json.loads(line).get('executable')]
    assert len(executables)==1,executables

    def save(name,value):
        (home/(name+'.json')).write_text(json.dumps(value,ensure_ascii=False,indent=2),encoding='utf-8')

    def edit(action,item='',block=0,expect_error=None,text='NATIVE-MEDIA-EDITED'):
        result=subprocess.run([executables[0],'edit::paginated::tests::paginated_native_media_fixture_command','--exact','--ignored','--nocapture'],
            cwd=REPO,env={**os.environ,'CC_NATIVE_MEDIA_HOME':str(home),'CC_NATIVE_MEDIA_ACTION':action,
                         'CC_NATIVE_MEDIA_ITEM':item,'CC_NATIVE_MEDIA_BLOCK':str(block),'CC_NATIVE_MEDIA_TEXT':text},capture_output=True,text=True,encoding='utf-8',check=True)
        with (home/'rust-commands.log').open('a',encoding='utf-8') as log:
            log.write(action+' '+item+'\n'+result.stdout+result.stderr)
        response=json.loads((home/'last-operation.json').read_text(encoding='utf-8'))
        if action!='diagnose':
            if expect_error:
                assert not response['ok'] and expect_error in response['error'],response
            else:
                assert response['ok'] and response['report']['status']=='committed_unverified',response
        return response

    def read_stage(name,expected=None):
        before=path.read_bytes()
        native=module.Native(binary,home)
        try:
            thread=native.call('thread/read',{'threadId':tid,'includeTurns':False})['thread']
            assert thread['id']==tid
            history={}
            for method in ['thread/items/list','thread/turns/list']:
                data,cursor=[],None
                while True:
                    params={'threadId':tid,'limit':3}
                    if cursor: params['cursor']=cursor
                    page=native.call(method,params)
                    data.extend(page['data'])
                    cursor=page.get('nextCursor')
                    if not cursor: break
                history[method]=data
        finally:
            native.close()
        assert path.read_bytes()==before,'read-only native reopen modified rollout'
        items=[row['item'] for row in history['thread/items/list']]
        if expected is not None: assert items==expected,name+' native item mismatch'
        save(name,{'threadId':tid,'thread':thread,**history,'rolloutSha256':hashlib.sha256(before).hexdigest()})
        print(name,'native read passed',flush=True)
        return items

    original=path.read_bytes()
    assert original==(home/'untouched-native.jsonl').read_bytes()
    baseline=read_stage('baseline')
    diagnosis=edit('diagnose')
    assert not diagnosis['capability']['blocked_reasons'],diagnosis['capability']['blocked_reasons']
    assert not diagnosis['capability']['diagnostics'],diagnosis['capability']['diagnostics']
    assert not diagnosis['history']['snapshots']
    save('baseline-mapping',diagnosis)
    stages=[]
    rows=[json.loads(line) for line in original.decode().splitlines()]
    for case in cases:
        raw=next(r for r in rows if r['payload'].get('turn_id')==case['turnId'] and (
            r['payload'].get('item',{}).get('delivery')=='async' if async_questions else r['payload'].get('item',{}).get('type')=='UserMessage'))
        item_id=raw['payload']['item']['id']
        positions=[i for i,b in enumerate(raw['payload']['item']['content']) if b['type'] in ['text','Text']]
        # Every text block is checked in unit tests; use the last block here to prove
        # the native integration does not move text to the first block.
        if positions:
            index=positions[-1]
            expected=json.loads(json.dumps(baseline))
            item=next(i for i in expected if i['id']==item_id)
            if async_questions:
                item['questions'][0]['title']='NATIVE-ASYNC-EDITED '+item['questions'][0]['title']
                if item['questions'][0].get('options'): item['questions'][0]['options'][-1]='UPDATED OPTION'
                item['text']='\n\n'.join('\n'.join([q['title']]+['- '+o for o in q.get('options') or []]) for q in item['questions'])
                new_text=item['text']
            else:
                new_text='NATIVE-MEDIA-EDITED'
                item['content'][index]['text']=new_text
                if 'text_elements' in item['content'][index]: item['content'][index]['text_elements']=[]
            report=edit('edit',item_id,index,text=new_text)
            save(case['case']+'-edit-report',report)
            read_stage(case['case']+'-edited',expected)
            save(case['case']+'-edit-undo-report',edit('undo'))
            assert path.read_bytes()==original,'edit undo did not restore original bytes'
            read_stage(case['case']+'-edit-undone',baseline)
        save(case['case']+'-delete-report',edit('delete',item_id))
        read_stage(case['case']+'-deleted',[i for i in baseline if i['id']!=item_id])
        after_rows=[json.loads(line) for line in path.read_text(encoding='utf-8').splitlines()]
        assert not any(r['payload'].get('item',{}).get('id')==item_id for r in after_rows)
        if async_questions:
            assert not any(r['payload'].get('call_id')==item_id for r in after_rows)
        else:
            assert not any(r['type']=='response_item' and r['payload'].get('role')=='user' and r['payload'].get('internal_chat_message_metadata_passthrough',{}).get('turn_id')==case['turnId'] and any(k.startswith('user.') for k in r['payload'].get('internal_chat_message_metadata_passthrough',{}).get('content_item_kinds',[])) for r in after_rows)
        save(case['case']+'-delete-undo-report',edit('undo'))
        assert path.read_bytes()==original,'delete undo did not restore original bytes'
        read_stage(case['case']+'-delete-undone',baseline)
        if async_questions:
            turn_items={r['payload']['item']['id'] for r in rows if r['payload'].get('type')=='item_completed' and r['payload'].get('turn_id')==case['turnId']}
            save(case['case']+'-delete-turn-report',edit('delete-turn',item_id))
            read_stage(case['case']+'-turn-deleted',[i for i in baseline if i['id'] not in turn_items])
            save(case['case']+'-turn-undo-report',edit('undo'))
            assert path.read_bytes()==original
            read_stage(case['case']+'-turn-undone',baseline)
            # A tool-emitted async question is a final_answer item in native
            # history, but must not become task_complete.last_agent_message.
            answer=next(r['payload']['item']['id'] for r in rows if r['payload'].get('type')=='item_completed'
                        and r['payload'].get('turn_id')==case['turnId'] and r['payload']['item'].get('type')=='AgentMessage'
                        and r['payload']['item'].get('delivery')!='async')
            save(case['case']+'-answer-delete-report',edit('delete',answer))
            read_stage(case['case']+'-answer-deleted',[i for i in baseline if i['id']!=answer])
            remaining=[json.loads(line) for line in path.read_text(encoding='utf-8').splitlines()]
            complete=next(r for r in remaining if r['payload'].get('type')=='task_complete' and r['payload'].get('turn_id')==case['turnId'])
            assert complete['payload'].get('last_agent_message') is None
            save(case['case']+'-answer-undo-report',edit('undo'))
            assert path.read_bytes()==original
            read_stage(case['case']+'-answer-undone',baseline)
        stages.append({'case':case['case'],'turnId':case['turnId'],'itemId':item_id,'textEdit':bool(positions),'delete':True,'undo':True,'nativeReopen':True})
    # Introduce a same-byte-length body mismatch in this isolated sample only.
    damaged=original.replace(b'SYNTHETIC QUESTION',b'SYNTHETIC QUESTIoN',1) if async_questions else original.replace(b'"text":"MIDDLE"',b'"text":"MIDDLe"',1)
    assert damaged!=original and len(damaged)==len(original)
    path.write_bytes(damaged)
    mismatch=edit('diagnose')
    save('intentional-mismatch',mismatch)
    assert not mismatch['capability']['blocked_reasons']
    assert len(mismatch['capability']['diagnostics'])==1 and 'EDIT_INCONSISTENT' in mismatch['capability']['diagnostics'][0]
    target=stages[0] if async_questions else next(s for s in stages if s['case']=='mixed-blocks')
    save('mismatch-edit-rejected',edit('edit',target['itemId'],0,'EDIT_INCONSISTENT'))
    save('mismatch-delete-rejected',edit('delete',target['itemId'],0,'EDIT_INCONSISTENT'))
    assert path.read_bytes()==damaged
    # Restore only the exact corruption injected above, not an editor snapshot.
    path.write_bytes(original)
    read_stage('restored-after-injection',baseline)
    final=edit('diagnose')
    assert not final['capability']['diagnostics']
    save('result',{'passed':True,'threadId':tid,'version':capture['version'],'sourceCommit':capture['sourceCommit'],'caseSet':capture.get('caseSet','media'),
        'baselineNativeGenerated':True,'stages':stages,'bodyMismatchDetected':True,'originalBytesRestored':True,
        'nativeReaderVersion':subprocess.check_output([str(binary),'--version'],text=True).strip(),
        'nativeColdReopen':True,'desktopGui':'NOT VERIFIED','realModelGeneration':False,'historicalToolReplay':False})


if __name__=='__main__':
    main()
