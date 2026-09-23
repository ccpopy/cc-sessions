-- Test fixture: openai/codex 8a3c4ea, thread_history migrations 1-6.
CREATE TABLE thread_turns (
    thread_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    rollout_ordinal INTEGER NOT NULL,
    status TEXT NOT NULL,
    error_json TEXT,
    started_at INTEGER,
    completed_at INTEGER,
    duration_ms INTEGER,
    first_user_item_id TEXT,
    final_agent_item_id TEXT,
    PRIMARY KEY (thread_id, turn_id)
);

CREATE UNIQUE INDEX idx_thread_turns_page
    ON thread_turns(thread_id, rollout_ordinal);

CREATE TABLE thread_items (
    thread_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    item_id TEXT NOT NULL,
    rollout_ordinal INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    item_json TEXT NOT NULL,
    PRIMARY KEY (thread_id, turn_id, item_id)
);

CREATE UNIQUE INDEX idx_thread_items_page
    ON thread_items(thread_id, rollout_ordinal);

CREATE INDEX idx_thread_items_by_turn_page
    ON thread_items(thread_id, turn_id, rollout_ordinal);

CREATE TABLE thread_history_projection_state (
    thread_id TEXT PRIMARY KEY,
    next_rollout_byte_offset INTEGER NOT NULL,
    next_rollout_ordinal INTEGER NOT NULL
);

ALTER TABLE thread_items ADD COLUMN item_type TEXT NOT NULL DEFAULT '';

UPDATE thread_items
SET item_type = json_extract(item_json, '$.type')
WHERE item_type = '';

CREATE INDEX idx_thread_items_user_messages
    ON thread_items(thread_id, rollout_ordinal)
    WHERE item_type = 'userMessage';

ALTER TABLE thread_turns ADD COLUMN rollout_byte_offset INTEGER;
ALTER TABLE thread_turns ADD COLUMN rollout_end_ordinal INTEGER;
ALTER TABLE thread_turns ADD COLUMN rollout_end_byte_offset INTEGER;

ALTER TABLE thread_items ADD COLUMN updated_at_ordinal INTEGER NOT NULL DEFAULT 0;

-- As of this migration, existing projected items originate from exactly one ItemCompleted rollout
-- event, so their creation ordinal is also their update ordinal.
UPDATE thread_items
SET updated_at_ordinal = rollout_ordinal;

-- Older writers can still append items with the zero default after this migration. Incremental
-- replay excludes those items until a newer writer projects an update for them.
-- Keep this index non-unique so mixed-version writers can continue to persist history.
CREATE INDEX idx_thread_items_updated_page
    ON thread_items(thread_id, updated_at_ordinal);

CREATE INDEX idx_thread_items_by_turn_updated_page
    ON thread_items(thread_id, turn_id, updated_at_ordinal);

CREATE TABLE thread_realtime_items (
    thread_id TEXT NOT NULL,
    item_id TEXT NOT NULL,
    rollout_ordinal INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    item_type TEXT NOT NULL,
    item_json TEXT NOT NULL,
    PRIMARY KEY (thread_id, item_id)
);

CREATE UNIQUE INDEX idx_thread_realtime_items_page
    ON thread_realtime_items(thread_id, rollout_ordinal);

CREATE INDEX idx_thread_realtime_items_boundary
    ON thread_realtime_items(thread_id, rollout_ordinal)
    WHERE item_type IN ('realtime_session_started', 'realtime_session_closed');

CREATE TRIGGER thread_realtime_items_projection_cleanup
    AFTER DELETE ON thread_history_projection_state
BEGIN
    DELETE FROM thread_realtime_items WHERE thread_id = OLD.thread_id;
END;

CREATE INDEX idx_thread_turns_end_page
    ON thread_turns(thread_id, rollout_end_ordinal, turn_id)
    WHERE rollout_end_ordinal IS NOT NULL;
