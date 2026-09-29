"""Remote session listings use the Codex index; mutations use the app-server protocol."""
import base64
import hashlib
import json
import fcntl
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile
import unittest
import uuid

SOURCE = Path(__file__).resolve().parents[1] / 'src-tauri/src/modules/remote_codex_sessions.rs'
SCRIPT = SOURCE.read_text().split('const REMOTE_SCRIPT: &str = r#"', 1)[1].split('"#;', 1)[0]
MOCK = '''#!/usr/bin/env python3
import json, os, pathlib, sqlite3, sys
home=pathlib.Path(os.environ['CODEX_HOME'])
state_path=home/'mock-state.json'
state=json.loads(state_path.read_text())
log=home/'mock-calls.jsonl'
def save(): state_path.write_text(json.dumps(state))
def sync_index(t):
    with sqlite3.connect(home/'state_5.sqlite') as db:
        db.execute('INSERT OR REPLACE INTO threads(id,rollout_path,name,title,cwd,updated_at,source,archived,has_user_event,preview) VALUES (?,?,?,?,?,?,?,?,?,?)',
                   (t['id'],state.get('wrongIndexPath') or t['path'],t.get('name'),t.get('name') or t['id'],t.get('cwd'),t['updatedAt'],json.dumps(t['source']),int(t['archived']),0,''))
def reply(id, result=None, error=None):
    print(json.dumps({'id':id,'error':{'message':error}} if error else {'id':id,'result':result or {}}),flush=True)
for line in sys.stdin:
    req=json.loads(line)
    if 'id' not in req: continue
    method=req['method']; p=req.get('params') or {}
    with log.open('a') as out: out.write(json.dumps({'method':method,'params':p})+'\\n')
    rid=req['id']
    if method=='initialize': reply(rid,{'capabilities':{}})
    elif method=='thread/list':
        rows=[v for v in state['threads'].values() if v['archived']==p.get('archived') and not v.get('hiddenFromList')]
        ancestor=p.get('ancestorThreadId')
        if ancestor:
            def descendant(t):
                parent=t.get('parentThreadId'); seen=set()
                while parent and parent not in seen:
                    if parent==ancestor: return True
                    seen.add(parent); parent=state['threads'].get(parent,{}).get('parentThreadId')
                return False
            rows=[v for v in rows if descendant(v)]
        rows.sort(key=lambda v:v['updatedAt'],reverse=True)
        offset=int(p.get('cursor') or 0); page=rows[offset:offset+2]
        reply(rid,{'data':page,'nextCursor':str(offset+2) if offset+2<len(rows) else None})
    elif method=='thread/read':
        t=state['threads'].get(p['threadId'])
        reply(rid,{'thread':t} if t else None, None if t else 'thread missing')
    elif method=='thread/delete':
        if state.get('deleteError') or p['threadId'] in state.get('deleteErrorIds',[]): reply(rid,error='forced delete failure'); continue
        root=p['threadId']; selected={root}; changed=True
        while changed:
            changed=False
            for sid,t in list(state['threads'].items()):
                if t.get('parentThreadId') in selected and sid not in selected:
                    selected.add(sid); changed=True
        for sid in selected:
            t=state['threads'].pop(sid,None)
            if t: pathlib.Path(t['path']).unlink(missing_ok=True)
        save(); reply(rid)
    elif method=='thread/resume':
        if state.get('resumeError'): reply(rid,error='forced resume failure'); continue
        sid=p['threadId']; found=list(home.glob('**/rollout-'+sid+'.jsonl'))
        found=[f for f in found if f.relative_to(home).parts[0] in ('sessions','archived_sessions')]
        if not found: reply(rid,error='rollout missing'); continue
        path=found[0]; meta=json.loads(path.read_text().splitlines()[0])['payload']
        if str(path.resolve())!=str(pathlib.Path(p.get('path','')).resolve()): reply(rid,error='resume path mismatch'); continue
        if path.relative_to(home).parts[0]=='archived_sessions': reply(rid,error='cannot resume archived rollout'); continue
        t={'id':sid,'name':meta.get('title',sid),'cwd':meta.get('cwd','/fixture'),'updatedAt':100,'source':meta.get('source','vscode'),'parentThreadId':meta.get('parentThreadId'),'path':str(path),'archived':False,'resumed':True}
        state['threads'][sid]=t; save(); sync_index(t); reply(rid,{'thread':t})
    elif method=='thread/name/set':
        state['threads'][p['threadId']]['name']=p['name']; save(); sync_index(state['threads'][p['threadId']]); reply(rid)
    elif method=='thread/archive':
        selected={p['threadId']}; changed=True
        while changed:
            changed=False
            for sid,t in state['threads'].items():
                if t.get('parentThreadId') in selected and sid not in selected: selected.add(sid); changed=True
        for sid in selected:
            t=state['threads'][sid]; t['archived']=True
            path=pathlib.Path(t['path'])
            if path.relative_to(home).parts[0]=='sessions':
                target=home/'archived_sessions'/path.relative_to(home/'sessions'); target.parent.mkdir(parents=True,exist_ok=True); path.rename(target); t['path']=str(target)
        save()
        for sid in selected: sync_index(state['threads'][sid])
        reply(rid)
    elif method=='thread/unarchive':
        sid=p['threadId']; t=state['threads'].get(sid)
        if t and t.get('resumed') and not t['archived']: reply(rid,error='active writer refuses unarchive'); continue
        if t: path=pathlib.Path(t['path'])
        else:
            found=list((home/'archived_sessions').glob('**/rollout-'+sid+'.jsonl'))
            if not found: reply(rid,error='archived rollout missing'); continue
            path=found[0]
        if path.relative_to(home).parts[0]=='archived_sessions':
            target=home/'sessions'/path.relative_to(home/'archived_sessions'); target.parent.mkdir(parents=True,exist_ok=True); path.rename(target); path=target
        if not t: t={'id':sid,'name':sid,'cwd':'/fixture','updatedAt':100,'source':'vscode','parentThreadId':None,'resumed':False}
        t['archived']=False; t['path']=str(path); state['threads'][sid]=t; save(); sync_index(t); reply(rid,{'thread':t})
    else: reply(rid,error='unexpected method '+method)
'''

class RemoteFixture(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.home = Path(self.temp.name)
        bindir = self.home / 'bin'; bindir.mkdir()
        codex = bindir / 'codex'; codex.write_text(MOCK); codex.chmod(0o755)
        self.env = dict(os.environ, HOME=str(self.home), PATH=str(bindir) + os.pathsep + os.environ.get('PATH',''))
        self.state = {'threads':{}}
        with sqlite3.connect(self.home/'state_5.sqlite') as db:
            db.execute('CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT, name TEXT, title TEXT, cwd TEXT, updated_at INTEGER, source TEXT, archived INTEGER, has_user_event INTEGER, preview TEXT)')
        self.save()
    def save(self): (self.home/'mock-state.json').write_text(json.dumps(self.state))
    def add(self, *, archived=False, source='vscode', parent=None, title=None, hidden=False):
        sid = str(uuid.uuid4())
        if parent and source == 'vscode': source={'subAgent':{'thread_spawn':{'parent_thread_id':parent}}}
        folder = self.home / ('archived_sessions' if archived else 'sessions') / '2026/09/28'; folder.mkdir(parents=True,exist_ok=True)
        path = folder / f'rollout-{sid}.jsonl'
        path.write_text(json.dumps({'type':'session_meta','payload':{'id':sid,'title':title or sid,'source':source,'parentThreadId':parent}})+'\n')
        self.state['threads'][sid]={'id':sid,'name':title or sid,'cwd':'/fixture','updatedAt':len(self.state['threads'])+100,'source':source,'parentThreadId':parent,'path':str(path),'archived':archived,'hiddenFromList':hidden}
        with sqlite3.connect(self.home/'state_5.sqlite') as db:
            db.execute('INSERT INTO threads VALUES (?,?,?,?,?,?,?,?,?,?)',(sid,str(path),title or sid,title or sid,'/fixture',len(self.state['threads'])+99,json.dumps(source),int(archived),0,''))
        self.save()
        return sid,path
    def run_action(self,action,sid=None,**kwargs):
        request={'codex_home':str(self.home),'action':action,**kwargs}
        if sid: request['id']=sid
        result=subprocess.run(['python3','-c',SCRIPT],input=json.dumps(request),text=True,capture_output=True,env=self.env,timeout=20,check=True)
        return json.loads(result.stdout)
    def calls(self):
        path=self.home/'mock-calls.jsonl'
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

class ProtocolTest(RemoteFixture):
    def test_readers_can_overlap_but_block_mutations(self):
        sid,path=self.add()
        descriptor=os.open(self.home,os.O_RDONLY)
        try:
            fcntl.flock(descriptor,fcntl.LOCK_SH|fcntl.LOCK_NB)
            for action in ('list','listTrash','location','readChunk'):
                with self.subTest(action=action):
                    self.assertTrue(self.run_action(action,sid)['ok'])
            for action in ('trash','restore'):
                self.assertFalse(self.run_action(action,sid)['ok'])
            self.assertTrue(path.exists())
            self.assertFalse(any(c['method']=='thread/delete' for c in self.calls()))
        finally:
            os.close(descriptor)

    def test_writer_blocks_reads_and_releases_lock(self):
        descriptor=os.open(self.home,os.O_RDONLY)
        try:
            fcntl.flock(descriptor,fcntl.LOCK_EX|fcntl.LOCK_NB)
            for action in ('list','trash','restore'):
                result=self.run_action(action)
                self.assertFalse(result['ok'])
                self.assertIn('占用',result['error'])
        finally:
            os.close(descriptor)
        self.assertTrue(self.run_action('list')['ok'])

    def test_list_includes_all_main_kinds_and_archive_states_without_official_listing(self):
        external,_=self.add(source='exec')
        conversation,_=self.add(source='vscode')
        archived,_=self.add(archived=True,source='exec')
        self.add(source={'subAgent':{'other':'review'}})
        result=self.run_action('list')
        self.assertTrue(result['ok'],result)
        self.assertEqual(result['result']['total'],3)
        self.assertEqual({row['id']:row['archived'] for row in result['result']['sessions']},
                         {external:False,conversation:False,archived:True})
        self.assertEqual(self.calls(),[])

    def test_unbounded_list_returns_more_than_one_thousand_main_sessions(self):
        rows=[]
        for index in range(1002):
            sid=str(uuid.uuid4())
            path=self.home/'sessions'/'2026/09/28'/f'rollout-{sid}.jsonl'
            rows.append((sid,str(path),f'Thread {index}',f'Thread {index}','/fixture',index,'"vscode"',0,0,''))
        with sqlite3.connect(self.home/'state_5.sqlite') as db:
            db.executemany('INSERT INTO threads VALUES (?,?,?,?,?,?,?,?,?,?)',rows)
        result=self.run_action('list')
        self.assertTrue(result['ok'],result)
        self.assertEqual(result['result']['total'],1002)
        self.assertEqual(len(result['result']['sessions']),1002)
        self.assertEqual(self.calls(),[])

    def test_list_includes_all_main_conversations_and_descendants_across_archive_states(self):
        older,_=self.add(title='Older main')
        current,_=self.add(title='Current main')
        child,_=self.add(parent=current,source={'subAgent':{'thread_spawn':{'parent_thread_id':current}}},hidden=True)
        grandchild,_=self.add(archived=True,parent=child,source={'subagent':{'thread_spawn':{'parent_thread_id':child}}},hidden=True)
        self.add(source={'subAgent':{'other':'orphan'}})
        response=self.run_action('list')
        self.assertTrue(response['ok'],response)
        rows=response['result']['sessions']
        self.assertEqual(response['result']['total'],2)
        self.assertEqual(rows[0]['id'],current)
        self.assertEqual({row['id']:row['parentThreadId'] for row in rows},
                         {older:None,current:None,child:current,grandchild:child})
        self.assertEqual(self.calls(),[])

    def test_direct_subagent_trash_is_rejected_before_backup_or_delete(self):
        root,_=self.add()
        child,path=self.add(parent=root,source={'subAgent':{'thread_spawn':{'parent_thread_id':root}}})
        response=self.run_action('trash',child)
        self.assertFalse(response['ok'])
        self.assertIn('subagent',response['error'])
        self.assertTrue(path.exists())
        self.assertFalse((self.home/'.antigravity_cockpit'/'cockpit-tools-codex-session-trash'/child).exists())
        self.assertFalse(any(call['method']=='thread/delete' for call in self.calls()))

    def test_nested_source_parent_relation_without_top_level_field(self):
        root,_=self.add()
        child,_=self.add(parent=root,source={'subagent':{'thread_spawn':{'parent_thread_id':root}}})
        self.state['threads'][child].pop('parentThreadId')
        self.save()
        result=self.run_action('list')
        self.assertTrue(result['ok'],result)
        rows={row['id']:row for row in result['result']['sessions']}
        self.assertEqual(rows[child]['parentThreadId'],root)
        self.assertEqual(result['result']['total'],1)

    def test_guardian_delete_failure_is_reported_and_backup_stays_pending(self):
        root, root_path = self.add()
        child, child_path = self.add(source={'subagent': {'other': 'guardian'}})
        meta = json.loads(child_path.read_text())
        meta['payload']['parent_thread_id'] = root
        child_path.write_text(json.dumps(meta) + '\n')
        self.state['deleteErrorIds'] = [child]
        self.save()
        response = self.run_action('trash', root)
        self.assertFalse(response['ok'])
        self.assertIn('family cleanup incomplete', response['error'])
        self.assertFalse(root_path.exists())
        self.assertTrue(child_path.exists())
        entry = self.home/'.antigravity_cockpit'/'cockpit-tools-codex-session-trash'/root
        self.assertTrue(json.loads((entry/'manifest.json').read_text())['deletionPending'])
        self.assertTrue((entry/'rollout.jsonl').exists())
        self.assertTrue((entry/('rollout-'+child+'.jsonl')).exists())

    def test_guardian_parent_from_rollout_is_listed_backed_up_and_deleted(self):
        root, root_path = self.add()
        guardian, guardian_path = self.add(source={'subagent': {'other': 'guardian'}})
        meta = json.loads(guardian_path.read_text())
        meta['payload']['parent_thread_id'] = root
        guardian_path.write_text(json.dumps(meta) + '\n')
        rows = self.run_action('list')['result']['sessions']
        self.assertEqual(next(row for row in rows if row['id'] == guardian)['parentThreadId'], root)
        response = self.run_action('trash', root)
        self.assertTrue(response['ok'], response)
        self.assertEqual(set(response['result']['sessionIds']), {root, guardian})
        self.assertFalse(root_path.exists())
        self.assertFalse(guardian_path.exists())
        self.assertEqual([call['params']['threadId'] for call in self.calls() if call['method'] == 'thread/delete'], [root, guardian])
        manifest = json.loads((self.home/'.antigravity_cockpit'/'cockpit-tools-codex-session-trash'/root/'manifest.json').read_text())
        self.assertFalse(manifest['deletionPending'])
        self.assertEqual(manifest['descendants'][0]['parentThreadId'], root)

    def test_trash_copies_descendants_then_official_delete_without_sql_mutation(self):
        root,root_path=self.add(title='Root title')
        child,child_path=self.add(parent=root,source={'subAgent':{'threadSpawn':{'parentThreadId':root}}},hidden=True)
        grand,grand_path=self.add(archived=True,parent=child,source={'subAgent':{'thread_spawn':{'parent_thread_id':child}}},hidden=True)
        sibling,sibling_path=self.add()
        response=self.run_action('trash',root)
        self.assertTrue(response['ok'],response)
        self.assertEqual(set(response['result']['sessionIds']),{root,child,grand})
        self.assertFalse(root_path.exists()); self.assertFalse(child_path.exists()); self.assertFalse(grand_path.exists())
        self.assertTrue(sibling_path.exists())
        manifest=json.loads((self.home/'.antigravity_cockpit'/'cockpit-tools-codex-session-trash'/root/'manifest.json').read_text())
        self.assertEqual({m['id'] for m in manifest['descendants']},{child,grand})
        trash=self.run_action('listTrash')['result']
        self.assertEqual({row['id'] for row in trash},{root,child,grand})
        self.assertEqual({row['id']:row['parentThreadId'] for row in trash},{root:None,child:root,grand:child})
        self.assertEqual({row['trashRootId'] for row in trash},{root})
        self.assertEqual({row['id']:row['archived'] for row in trash},{root:False,child:False,grand:True})
        self.assertEqual({row['sessionKind'] for row in trash},{'conversation','subagent'})
        self.assertEqual([c['params']['threadId'] for c in self.calls() if c['method']=='thread/delete'],[root])
        self.assertFalse(any(c['method']=='thread/list' for c in self.calls()))
        db=sqlite3.connect(self.home/'state_5.sqlite')
        self.assertEqual({row[0] for row in db.execute('SELECT id FROM threads')},{root,child,grand,sibling})
        db.close()
        restored=self.run_action('restore',root)
        self.assertTrue(restored['ok'],restored)
        self.assertTrue(root_path.exists()); self.assertTrue(child_path.exists()); self.assertTrue(grand_path.exists())
        self.assertTrue(self.state_file()['threads'][grand]['archived'])
        self.assertTrue(any(c['method']=='thread/name/set' for c in self.calls()))
        self.assertFalse((self.home/'.antigravity_cockpit'/'cockpit-tools-codex-session-trash'/root).exists())

    def state_file(self): return json.loads((self.home/'mock-state.json').read_text())

    def test_delete_failure_keeps_original_and_removes_temporary_backup(self):
        sid,path=self.add(); self.state['deleteError']=True; self.save()
        response=self.run_action('trash',sid)
        self.assertFalse(response['ok'])
        self.assertIn('forced delete failure',response['error'])
        self.assertTrue(path.exists())
        self.assertFalse((self.home/'.antigravity_cockpit'/'cockpit-tools-codex-session-trash'/sid).exists())
        self.assertEqual(self.run_action('listTrash')['result'], [])

    def test_trash_accepts_continuation_filename_and_rejects_wrong_root(self):
        sid,path=self.add()
        segment=str(uuid.uuid4())
        new_path=path.with_name(f'rollout-2025-01-02T03-04-05-{sid}_{segment}.jsonl')
        path.rename(new_path)
        self.state['threads'][sid]['path']=str(new_path); self.save()
        with sqlite3.connect(self.home/'state_5.sqlite') as db:
            db.execute('UPDATE threads SET rollout_path=? WHERE id=?',(str(new_path),sid))
        self.assertTrue(self.run_action('trash',sid)['ok'])
        self.assertEqual([row['id'] for row in self.run_action('listTrash')['result']],[sid])
        manifest=self.home/'.antigravity_cockpit'/'cockpit-tools-codex-session-trash'/sid/'manifest.json'
        data=json.loads(manifest.read_text()); original=data['relativePath']
        data['relativePath']=original.replace(sid,str(uuid.uuid4())); manifest.write_text(json.dumps(data))
        self.assertFalse(self.run_action('purge',sid)['ok'])
        data['relativePath']=original; manifest.write_text(json.dumps(data))
        self.assertTrue(self.run_action('clearTrash')['ok'])
        self.assertFalse(manifest.exists())

    def test_pending_backup_is_not_trash_and_cannot_be_restored_or_purged(self):
        sid,path=self.add()
        self.assertTrue(self.run_action('trash',sid)['ok'])
        entry=self.home/'.antigravity_cockpit'/'cockpit-tools-codex-session-trash'/sid
        manifest=entry/'manifest.json'
        data=json.loads(manifest.read_text()); data['deletionPending']=True
        manifest.write_text(json.dumps(data))
        self.assertEqual(self.run_action('listTrash')['result'],[])
        self.assertFalse(self.run_action('restore',sid)['ok'])
        self.assertTrue(self.run_action('clearTrash')['ok'])
        self.assertTrue(entry.exists())

    def test_restore_failure_preserves_backup(self):
        sid,path=self.add()
        self.assertTrue(self.run_action('trash',sid)['ok'])
        state=self.state_file(); state['resumeError']=True
        (self.home/'mock-state.json').write_text(json.dumps(state))
        response=self.run_action('restore',sid)
        self.assertFalse(response['ok'])
        self.assertTrue(path.exists())
        self.assertTrue((self.home/'.antigravity_cockpit'/'cockpit-tools-codex-session-trash'/sid/'rollout.jsonl').exists())

    def test_restore_checks_final_indexed_file_before_removing_backup(self):
        sid,_=self.add()
        other,other_path=self.add()
        self.assertTrue(self.run_action('trash',sid)['ok'])
        state=self.state_file(); state['wrongIndexPath']=str(other_path)
        (self.home/'mock-state.json').write_text(json.dumps(state))
        result=self.run_action('restore',sid)
        self.assertFalse(result['ok'])
        self.assertIn('wrong session ID',result['error'])
        self.assertTrue((self.home/'.antigravity_cockpit'/'cockpit-tools-codex-session-trash'/sid/'rollout.jsonl').exists())
        self.assertEqual(json.loads(other_path.read_text().splitlines()[0])['payload']['id'],other)

    def test_purge_only_selected_root_and_clear_trash(self):
        root,_=self.add()
        child,_=self.add(parent=root)
        sibling,_=self.add()
        self.assertTrue(self.run_action('trash',root)['ok'])
        self.assertTrue(self.run_action('trash',sibling)['ok'])
        self.assertFalse(self.run_action('purge',child)['ok'])
        before=len([call for call in self.calls() if call['method']=='thread/delete'])
        self.assertEqual(self.run_action('purge',root)['result'],{'purgedRoots':1,'purgedSessions':2})
        self.assertEqual([row['id'] for row in self.run_action('listTrash')['result']],[sibling])
        self.assertEqual(self.run_action('clearTrash')['result'],{'purgedRoots':1,'purgedSessions':1})
        self.assertEqual(self.run_action('listTrash')['result'],[])
        self.assertEqual(len([call for call in self.calls() if call['method']=='thread/delete']),before)

    def test_malformed_trash_path_blocks_purge_and_clear(self):
        sid,_=self.add()
        self.assertTrue(self.run_action('trash',sid)['ok'])
        entry=self.home/'.antigravity_cockpit'/'cockpit-tools-codex-session-trash'/sid
        manifest=json.loads((entry/'manifest.json').read_text())
        manifest['relativePath']='sessions/../outside/rollout-'+sid+'.jsonl'
        (entry/'manifest.json').write_text(json.dumps(manifest))
        self.assertFalse(self.run_action('purge',sid)['ok'])
        self.assertFalse(self.run_action('clearTrash')['ok'])
        self.assertTrue(entry.exists())

    def test_trash_accepts_official_timestamped_rollout_paths(self):
        sid,_=self.add()
        self.assertTrue(self.run_action('trash',sid)['ok'])
        entry=self.home/'.antigravity_cockpit'/'cockpit-tools-codex-session-trash'/sid
        manifest=json.loads((entry/'manifest.json').read_text())
        manifest['relativePath']='sessions/2026/01/02/rollout-2026-01-02T03-04-05-'+sid+'.jsonl'
        (entry/'manifest.json').write_text(json.dumps(manifest))
        result=self.run_action('listTrash')
        self.assertTrue(result['ok'],result)
        self.assertEqual([row['id'] for row in result['result']],[sid])
        manifest['relativePath']='sessions/rollout-2026-01-02T03-04-05-'+str(uuid.uuid4())+'.jsonl'
        (entry/'manifest.json').write_text(json.dumps(manifest))
        self.assertFalse(self.run_action('purge',sid)['ok'])
        self.assertTrue((entry/'rollout.jsonl').is_file())

    def test_import_stages_family_and_registers_with_official_protocol(self):
        transfer=str(uuid.uuid4())
        root=str(uuid.uuid4()); child=str(uuid.uuid4())
        members=[]
        for sid,parent,archived in ((root,None,False),(child,root,True)):
            content=(json.dumps({'type':'session_meta','payload':{'id':sid,'title':sid,'cwd':'/fixture','source':{'subAgent':{'thread_spawn':{'parent_thread_id':root}}} if parent else 'vscode','parentThreadId':parent}})+'\n').encode()
            self.assertTrue(self.run_action('importStage',sid,transferId=transfer,offset=0,totalBytes=len(content),data=base64.b64encode(content).decode())['ok'])
            members.append({'id':sid,'title':sid,'relativePath':('archived_sessions' if archived else 'sessions')+'/2026/09/28/rollout-'+sid+'.jsonl','archived':archived,'sha256':hashlib.sha256(content).hexdigest(),'sizeBytes':len(content)})
        result=self.run_action('importCommit',transferId=transfer,members=list(reversed(members)))
        self.assertTrue(result['ok'],result)
        self.assertEqual(result['result']['importedCount'],2)
        self.assertFalse((self.home/'cockpit-tools-remote-session-import'/transfer).exists())
        self.assertFalse((self.home/'cockpit-tools-remote-session-import').exists())
        self.assertTrue(self.state_file()['threads'][child]['archived'])
        calls=[call['params']['threadId'] for call in self.calls() if call['method']=='thread/resume']
        self.assertEqual(calls,[root,child])

    def test_import_archived_parent_before_active_child(self):
        transfer=str(uuid.uuid4()); root=str(uuid.uuid4()); child=str(uuid.uuid4())
        members=[]
        for sid,parent,archived in ((root,None,True),(child,root,False)):
            content=(json.dumps({'type':'session_meta','payload':{'id':sid,'parentThreadId':parent,'source':{'subAgent':{'thread_spawn':{'parent_thread_id':root}}} if parent else 'vscode'}})+'\n').encode()
            self.assertTrue(self.run_action('importStage',sid,transferId=transfer,offset=0,totalBytes=len(content),data=base64.b64encode(content).decode())['ok'])
            members.append({'id':sid,'title':sid,'relativePath':('archived_sessions' if archived else 'sessions')+'/2026/09/28/rollout-'+sid+'.jsonl','archived':archived,'sha256':hashlib.sha256(content).hexdigest(),'sizeBytes':len(content)})
        result=self.run_action('importCommit',transferId=transfer,members=list(reversed(members)))
        self.assertTrue(result['ok'],result)
        threads=self.state_file()['threads']
        self.assertTrue(threads[root]['archived'])
        self.assertFalse(threads[child]['archived'])
        calls=self.calls()
        self.assertLess(next(i for i,c in enumerate(calls) if c['method']=='thread/archive' and c['params']['threadId']==root),next(i for i,c in enumerate(calls) if c['method']=='thread/resume' and c['params']['threadId']==child))

    def test_import_rejects_existing_id_and_malformed_path(self):
        existing,_=self.add()
        transfer=str(uuid.uuid4())
        content=(json.dumps({'type':'session_meta','payload':{'id':existing}})+'\n').encode()
        self.assertTrue(self.run_action('importStage',existing,transferId=transfer,offset=0,totalBytes=len(content),data=base64.b64encode(content).decode())['ok'])
        member={'id':existing,'title':existing,'relativePath':'sessions/2026/09/28/rollout-'+existing+'.jsonl','archived':False,'sha256':hashlib.sha256(content).hexdigest(),'sizeBytes':len(content)}
        self.assertFalse(self.run_action('importCommit',transferId=transfer,members=[member])['ok'])
        member['id']=str(uuid.uuid4())
        member['relativePath']='sessions/../outside/rollout-'+member['id']+'.jsonl'
        self.assertFalse(self.run_action('importCommit',transferId=transfer,members=[member])['ok'])
        self.assertTrue(self.run_action('importAbort',transferId=transfer)['ok'])
        self.assertFalse((self.home/'cockpit-tools-remote-session-import'/transfer).exists())
        self.assertFalse((self.home/'cockpit-tools-remote-session-import').exists())

    def test_import_rejects_bad_checksum_and_file_metadata_id(self):
        actual=str(uuid.uuid4()); requested=str(uuid.uuid4()); transfer=str(uuid.uuid4())
        content=(json.dumps({'type':'session_meta','payload':{'id':actual}})+'\n').encode()
        self.assertTrue(self.run_action('importStage',requested,transferId=transfer,offset=0,totalBytes=len(content),data=base64.b64encode(content).decode())['ok'])
        member={'id':requested,'title':requested,'relativePath':'sessions/2026/09/28/rollout-'+requested+'.jsonl','archived':False,'sha256':'0'*64,'sizeBytes':len(content)}
        bad_hash=self.run_action('importCommit',transferId=transfer,members=[member])
        self.assertFalse(bad_hash['ok'])
        self.assertIn('checksum',bad_hash['error'])
        member['sha256']=hashlib.sha256(content).hexdigest()
        bad_id=self.run_action('importCommit',transferId=transfer,members=[member])
        self.assertFalse(bad_id['ok'])
        self.assertIn('session ID mismatch',bad_id['error'])
        self.assertFalse((self.home/'sessions/2026/09/28'/f'rollout-{requested}.jsonl').exists())
        self.assertTrue(self.run_action('importAbort',transferId=transfer)['ok'])

    def test_restore_retry_uses_each_member_path_and_reconciles_mixed_archive(self):
        root,root_path=self.add(title='Parent')
        child,child_path=self.add(parent=root,title='Child')
        self.assertTrue(self.run_action('trash',root)['ok'])
        state=self.state_file(); state['resumeError']=True
        (self.home/'mock-state.json').write_text(json.dumps(state))
        failed=self.run_action('restore',root)
        self.assertFalse(failed['ok'])
        self.assertTrue(root_path.exists()); self.assertTrue(child_path.exists())
        self.assertTrue((self.home/'.antigravity_cockpit'/'cockpit-tools-codex-session-trash'/root/'rollout.jsonl').exists())
        state=self.state_file(); state.pop('resumeError')
        (self.home/'mock-state.json').write_text(json.dumps(state))
        restored=self.run_action('restore',root)
        self.assertTrue(restored['ok'],restored)
        threads=self.state_file()['threads']
        self.assertFalse(threads[root]['archived'])
        self.assertFalse(threads[child]['archived'])
        resume=[c for c in self.calls() if c['method']=='thread/resume']
        self.assertEqual({c['params']['threadId']:Path(c['params']['path']) for c in resume},{root:root_path.resolve(),child:child_path.resolve()})

    def test_archived_parent_processed_before_active_child(self):
        root,_=self.add(archived=True,title='Parent')
        child,_=self.add(parent=root,title='Child')
        self.assertTrue(self.run_action('trash',root)['ok'])
        result=self.run_action('restore',root)
        self.assertTrue(result['ok'],result)
        threads=self.state_file()['threads']
        self.assertTrue(threads[root]['archived'])
        self.assertFalse(threads[child]['archived'])
        calls=self.calls()
        self.assertLess(next(i for i,c in enumerate(calls) if c['method']=='thread/archive' and c['params']['threadId']==root),next(i for i,c in enumerate(calls) if c['method']=='thread/resume' and c['params']['threadId']==child))
        self.assertFalse(any(c['method']=='thread/unarchive' and c['params']['threadId']==child for c in calls))

    def test_legacy_manifest_restores_without_database_writes(self):
        sid,path=self.add(archived=True)
        content=path.read_bytes(); path.unlink()
        self.state['threads'].pop(sid); self.save()
        entry=self.home/'.antigravity_cockpit'/'cockpit-tools-codex-session-trash'/sid; entry.mkdir(parents=True)
        (entry/'rollout.jsonl').write_bytes(content)
        (entry/'manifest.json').write_text(json.dumps({'id':sid,'title':'legacy','archived':True,'deletedAt':100,'relativePath':path.relative_to(self.home).as_posix(),'dbName':'state_5.sqlite','dbRows':{'threads':[{'id':sid}]},'dbEdges':[],'descendants':[]}))
        result=self.run_action('restore',sid)
        self.assertTrue(result['ok'],result)
        self.assertEqual(path.read_bytes(),content)
        self.assertTrue(self.state_file()['threads'][sid]['archived'])
        db=sqlite3.connect(self.home/'state_5.sqlite')
        self.assertEqual(db.execute('SELECT archived FROM threads WHERE id=?',(sid,)).fetchone(),(1,))
        db.close()

    def test_legacy_child_manifest_cannot_be_restored_directly(self):
        root,_=self.add()
        child,path=self.add(parent=root,source={'subagent':{'thread_spawn':{'parent_thread_id':root}}})
        content=path.read_bytes(); path.unlink()
        self.state['threads'].pop(child); self.save()
        entry=self.home/'.antigravity_cockpit'/'cockpit-tools-codex-session-trash'/child; entry.mkdir(parents=True)
        (entry/'rollout.jsonl').write_bytes(content)
        (entry/'manifest.json').write_text(json.dumps({'id':child,'title':'legacy child','archived':False,'deletedAt':100,'relativePath':path.relative_to(self.home).as_posix(),'descendants':[]}))
        result=self.run_action('restore',child)
        self.assertFalse(result['ok'])
        self.assertIn('subagent',result['error'])
        purged=self.run_action('purge',child)
        self.assertFalse(purged['ok'])
        self.assertIn('subagent',purged['error'])
        self.assertFalse(path.exists())
        self.assertTrue((entry/'rollout.jsonl').exists())

    def test_read_chunk_and_location_preserve_raw_rollout(self):
        sid,path=self.add()
        self.assertEqual(self.run_action('location',sid)['result']['path'],str(path.resolve()))
        chunk=self.run_action('readChunk',sid)['result']
        self.assertEqual(chunk['totalBytes'],path.stat().st_size)
        self.assertEqual(base64.b64decode(chunk['data']),path.read_bytes())

    def test_stale_rollout_does_not_hide_indexed_sessions(self):
        sid,path=self.add()
        path.unlink()
        result=self.run_action('list')
        self.assertTrue(result['ok'],result)
        self.assertEqual(result['result']['sessions'][0]['id'],sid)
        self.assertEqual(result['result']['sessions'][0]['sizeBytes'],0)
        self.assertEqual(self.calls(),[])

    def test_absent_index_returns_empty_without_app_server(self):
        (self.home/'state_5.sqlite').unlink()
        result=self.run_action('list')
        self.assertEqual(result,{'ok':True,'result':{'sessions':[],'total':0}})
        self.assertEqual(self.calls(),[])

    def test_existing_ids_includes_orphan_subagents_omitted_from_grouped_list(self):
        orphan,_=self.add(source={'subAgent':{'other':'orphan'}})
        self.assertEqual(self.run_action('list')['result']['sessions'],[])
        self.assertIn(orphan,self.run_action('existingIds')['result'])

    def test_highest_numeric_index_and_older_schema_without_name(self):
        self.add(title='Lower version')
        sid,path=self.add(title='  Index title  ')
        with sqlite3.connect(self.home/'state_12.sqlite') as db:
            db.execute('CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT, title TEXT, cwd TEXT, updated_at INTEGER, source TEXT, archived INTEGER)')
            db.execute('INSERT INTO threads VALUES (?,?,?,?,?,?,?)',(sid,str(path),'  Index title  ','/higher',200,'"exec"',0))
        result=self.run_action('list')
        self.assertTrue(result['ok'],result)
        self.assertEqual([(row['id'],row['title'],row['sessionKind']) for row in result['result']['sessions']],[(sid,'Index title','external')])
        self.assertEqual(self.calls(),[])

    def test_corrupt_or_incompatible_index_reports_error_without_fallback(self):
        sid,_=self.add()
        newer=self.home/'state_12.sqlite'
        newer.write_bytes(b'not a database')
        corrupt=self.run_action('list')
        self.assertFalse(corrupt['ok'])
        self.assertIn('Cannot read Codex session index',corrupt['error'])
        newer.unlink()
        with sqlite3.connect(newer) as db: db.execute('CREATE TABLE threads(id TEXT PRIMARY KEY)')
        incompatible=self.run_action('list')
        self.assertFalse(incompatible['ok'])
        self.assertIn('incompatible threads schema',incompatible['error'])
        self.assertEqual(self.calls(),[])
        with sqlite3.connect(self.home/'state_5.sqlite') as db:
            self.assertEqual(db.execute('SELECT id FROM threads').fetchone()[0],sid)

if __name__=='__main__': unittest.main()
