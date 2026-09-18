CREATE TABLE messages (
    id BIGINT PRIMARY KEY,
    guild_id BIGINT NOT NULL,
    channel_id BIGINT NOT NULL,
    author_id BIGINT NOT NULL,
    content TEXT NOT NULL,
    timestamp TIMESTAMPTZ NOT NULL,
    edited_timestamp TIMESTAMPTZ,
    images JSONB NOT NULL DEFAULT '[]'::jsonb CHECK (jsonb_typeof(images) = 'array')
);

-- The prefix supports guild/channel lookups; the remaining keys support retention.
CREATE INDEX messages_guild_channel_timestamp_idx
    ON messages (guild_id, channel_id, timestamp DESC, id DESC);
CREATE INDEX messages_timestamp_idx ON messages (timestamp DESC);

-- Serialize inserts in each channel before the retention trigger runs. At the
-- application's READ COMMITTED isolation level, pruning sees committed peers.
CREATE FUNCTION lock_message_channel() RETURNS TRIGGER
LANGUAGE plpgsql AS $$
BEGIN
    PERFORM pg_advisory_xact_lock(NEW.channel_id);
    RETURN NEW;
END;
$$;

CREATE TRIGGER messages_lock_channel
    BEFORE INSERT ON messages
    FOR EACH ROW EXECUTE FUNCTION lock_message_channel();

CREATE FUNCTION prune_channel_messages() RETURNS TRIGGER
LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM messages
    WHERE id IN (
        SELECT id FROM messages
        WHERE guild_id = NEW.guild_id AND channel_id = NEW.channel_id
        ORDER BY timestamp DESC, id DESC
        OFFSET 500
    );
    RETURN NULL;
END;
$$;

CREATE TRIGGER messages_prune_channel
    AFTER INSERT ON messages
    FOR EACH ROW EXECUTE FUNCTION prune_channel_messages();

CREATE TABLE strikes (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    guild_id BIGINT NOT NULL,
    channel_id BIGINT NOT NULL,
    user_id BIGINT NOT NULL,
    moderator_id BIGINT NOT NULL,
    reason TEXT NOT NULL CHECK (char_length(btrim(reason)) BETWEEN 1 AND 1000),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Retried gateway interactions must not create duplicate strikes.
    interaction_id BIGINT UNIQUE,
    -- Keep automatic strike sources after messages are pruned or rules deleted.
    source_message_id BIGINT,
    source_action_id INTEGER,
    CONSTRAINT strikes_source_check CHECK (
        (interaction_id IS NOT NULL AND source_message_id IS NULL AND source_action_id IS NULL)
        OR (interaction_id IS NULL AND source_message_id IS NOT NULL AND source_action_id IS NOT NULL)
    )
);

CREATE INDEX strikes_guild_user_created_idx
    ON strikes (guild_id, user_id, created_at DESC, id DESC);
CREATE UNIQUE INDEX strikes_message_action_idx
    ON strikes (guild_id, source_message_id, source_action_id)
    WHERE source_message_id IS NOT NULL;

CREATE TABLE message_actions (
    id SERIAL PRIMARY KEY,
    guild_id BIGINT NOT NULL,
    only_channels BIGINT[],
    question TEXT NOT NULL,
    code TEXT,
    -- Nullable for manually inserted rules; deduplicates admin command retries.
    created_by_interaction_id BIGINT UNIQUE
);

CREATE INDEX message_actions_guild_id_idx ON message_actions (guild_id);

CREATE TABLE strike_actions (
    id SERIAL PRIMARY KEY,
    guild_id BIGINT NOT NULL,
    only_channels BIGINT[],
    question TEXT NOT NULL,
    code TEXT,
    created_by_interaction_id BIGINT UNIQUE
);

CREATE INDEX strike_actions_guild_id_idx ON strike_actions (guild_id);
