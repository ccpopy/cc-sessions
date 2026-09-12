PRAGMA foreign_keys = ON;
CREATE TABLE project (id TEXT PRIMARY KEY);
INSERT INTO project VALUES ('project-fixture');
CREATE TABLE session (
 id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES project(id),
 parent_id TEXT, workspace_id TEXT, slug TEXT NOT NULL, directory TEXT NOT NULL,
 path TEXT, title TEXT NOT NULL, version TEXT NOT NULL, metadata TEXT,
 share_url TEXT, permission TEXT, revert TEXT, agent TEXT, model TEXT,
 summary_additions INTEGER, summary_deletions INTEGER, summary_files INTEGER, summary_diffs TEXT,
 cost REAL NOT NULL DEFAULT 0, tokens_input INTEGER NOT NULL DEFAULT 0,
 tokens_output INTEGER NOT NULL DEFAULT 0, tokens_reasoning INTEGER NOT NULL DEFAULT 0,
 tokens_cache_read INTEGER NOT NULL DEFAULT 0, tokens_cache_write INTEGER NOT NULL DEFAULT 0,
 time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
 time_archived INTEGER, time_compacting INTEGER
);
CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES session(id),
 time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL REFERENCES message(id),
 session_id TEXT NOT NULL, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT NOT NULL);
CREATE TABLE event_sequence (aggregate_id TEXT PRIMARY KEY, seq INTEGER NOT NULL, owner_id TEXT);
CREATE TABLE event (id TEXT PRIMARY KEY, aggregate_id TEXT NOT NULL REFERENCES event_sequence(aggregate_id),
 seq INTEGER NOT NULL, type TEXT NOT NULL, data TEXT NOT NULL, UNIQUE(aggregate_id,seq));
CREATE TABLE todo (session_id TEXT, content TEXT);
CREATE TABLE session_share (session_id TEXT, secret TEXT);
CREATE TABLE account (id TEXT, access_token TEXT);
CREATE TABLE session_input (session_id TEXT, prompt TEXT);
CREATE TABLE session_message (session_id TEXT, data TEXT);
INSERT INTO session (id,project_id,slug,directory,path,title,version,metadata,workspace_id,parent_id,
 share_url,permission,revert,agent,model,cost,tokens_input,time_created,time_updated,time_archived,time_compacting)
 VALUES ('ses_fixture','project-fixture','source','/fixture','subdir','Copy fixture','1.18.30',
 '{"nested":{"keep":true}}','workspace-fixture','ses_parent','https://example.invalid/shared',
 '[{"action":"allow"}]','{"messageID":"msg_a0-after-wrap"}','source-agent','{"id":"source-model"}',999,999,1,2,3,4);
INSERT INTO session (id,project_id,slug,directory,title,version,time_created,time_updated)
 VALUES ('ses_other','project-fixture','other','/other','Unrelated session','1.18.30',1,1);
INSERT INTO event_sequence VALUES ('ses_fixture',0,'private-owner');
INSERT INTO event VALUES ('evt_source','ses_fixture',0,'session.created.1','{"sentinel":true}');
INSERT INTO todo VALUES ('ses_fixture','Do not inherit');
INSERT INTO session_share VALUES ('ses_fixture','private-share-secret');
INSERT INTO account VALUES ('account-fixture','private-token');
INSERT INTO session_input VALUES ('ses_fixture','Pending input: do not inherit');
