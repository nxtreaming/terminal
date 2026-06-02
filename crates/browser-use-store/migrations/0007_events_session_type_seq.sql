CREATE INDEX IF NOT EXISTS events_session_type_seq_idx ON events(session_id, type, seq DESC);
