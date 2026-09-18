CREATE TABLE twitch_channels (
    bot_user_id TEXT NOT NULL,
    channel_id TEXT NOT NULL,
    login TEXT NOT NULL,
    joined_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (bot_user_id, channel_id)
);
