ALTER TABLE agent_messages ADD COLUMN consumed_ms INTEGER;

CREATE INDEX IF NOT EXISTS agent_messages_target_pending_created_idx
    ON agent_messages(target_session_id, consumed_ms, created_ms);
