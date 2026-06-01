ALTER TABLE agent_messages ADD COLUMN input_items TEXT;
ALTER TABLE agent_messages ADD COLUMN input_kind TEXT NOT NULL DEFAULT 'inter_agent';
