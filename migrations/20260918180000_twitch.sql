-- Twitch IDs (including UUID message IDs) never share Discord's snowflake namespace.
CREATE TABLE twitch_rules (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    channel_id TEXT NOT NULL,
    condition TEXT NOT NULL CHECK (char_length(btrim(condition)) BETWEEN 1 AND 1000),
    action TEXT NOT NULL CHECK (action IN ('delete', 'timeout', 'ban', 'strike')),
    timeout_seconds INTEGER,
    strike_threshold INTEGER CHECK (strike_threshold > 0),
    created_by_message_id TEXT NOT NULL,
    UNIQUE (channel_id, created_by_message_id),
    CHECK ((action = 'timeout' AND timeout_seconds BETWEEN 1 AND 1209600)
        OR (action <> 'timeout' AND timeout_seconds IS NULL)),
    CHECK (action <> 'timeout' OR timeout_seconds IS NOT NULL),
    CHECK (strike_threshold IS NULL OR action IN ('timeout', 'ban'))
);
CREATE INDEX twitch_rules_channel_idx ON twitch_rules (channel_id, id);

CREATE TABLE twitch_messages (
    channel_id TEXT NOT NULL,
    id TEXT NOT NULL,
    author_id TEXT NOT NULL,
    content TEXT NOT NULL,
    timestamp TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (channel_id, id)
);
CREATE INDEX twitch_messages_history_idx
    ON twitch_messages (channel_id, timestamp DESC, id DESC);

CREATE TABLE twitch_strikes (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    channel_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    reason TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    source_message_id TEXT NOT NULL,
    source_rule_id BIGINT NOT NULL,
    -- Keep the source after removal so redelivery cannot recreate a removed strike.
    removed_at TIMESTAMPTZ,
    UNIQUE (channel_id, source_message_id, source_rule_id)
);
CREATE INDEX twitch_strikes_user_idx
    ON twitch_strikes (channel_id, user_id, id) WHERE removed_at IS NULL;

-- Deduplication survives chat deletion and message-history pruning.
CREATE TABLE twitch_receipts (
    channel_id TEXT NOT NULL,
    event_id TEXT NOT NULL,
    received_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (channel_id, event_id)
);
CREATE INDEX twitch_receipts_age_idx ON twitch_receipts (received_at);
