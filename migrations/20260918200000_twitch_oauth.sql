-- OAuth credentials are server-only and never returned by the public HTTP service.
CREATE TABLE twitch_oauth_tokens (
    client_id TEXT PRIMARY KEY,
    bot_user_id TEXT NOT NULL,
    bot_login TEXT NOT NULL,
    access_token TEXT NOT NULL,
    refresh_token TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
