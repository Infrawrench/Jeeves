ALTER TABLE message_actions ADD COLUMN role_id BIGINT CHECK (role_id > 0 AND role_id <> guild_id);
ALTER TABLE strike_actions ADD COLUMN role_id BIGINT CHECK (role_id > 0 AND role_id <> guild_id);
