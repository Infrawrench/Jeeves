use std::{env, num::NonZeroU32};

use anyhow::{Context, Result, ensure};
use sqlx::{ConnectOptions, postgres::PgConnectOptions};
use url::Url;

// Do not derive Debug: this contains credentials.
pub struct Config {
    pub discord_token: Option<String>,
    pub twitch: Option<crate::twitch::Config>,
    pub gemini_model: String,
    pub gemini_backend: GeminiBackend,
    pub database: PgConnectOptions,
    pub database_max_connections: u32,
}

pub enum GeminiBackend {
    Developer { api_key: String },
    Vertex { project: String, location: String },
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let discord_token = optional("DISCORD_TOKEN")?;
        let twitch = crate::twitch::Config::from_env()?;
        ensure!(
            discord_token.is_some() || twitch.is_some(),
            "Configure DISCORD_TOKEN or Twitch; see .env.example"
        );
        let gemini_model = optional("GEMINI_MODEL")?.unwrap_or_else(|| "gemini-3.7-flash".into());
        let gemini_backend = match optional("GEMINI_BACKEND")?
            .as_deref()
            .unwrap_or("developer")
        {
            "developer" => GeminiBackend::Developer {
                api_key: required("GEMINI_API_KEY")?,
            },
            "vertex" => GeminiBackend::Vertex {
                project: required("GOOGLE_CLOUD_PROJECT")?,
                location: optional("GOOGLE_CLOUD_LOCATION")?.unwrap_or_else(|| "global".into()),
            },
            _ => anyhow::bail!("GEMINI_BACKEND must be developer or vertex"),
        };
        ensure!(
            gemini_model
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b)),
            "GEMINI_MODEL must be a model ID, such as gemini-3.7-flash"
        );
        let database = database_options(&required("DATABASE_URL")?)?;
        let database_max_connections = optional("DATABASE_MAX_CONNECTIONS")?
            .unwrap_or_else(|| "5".into())
            .parse::<NonZeroU32>()
            .context("DATABASE_MAX_CONNECTIONS must be a positive integer")?
            .get();

        Ok(Self {
            discord_token,
            twitch,
            gemini_model,
            gemini_backend,
            database,
            database_max_connections,
        })
    }
}

pub(crate) fn database_options(value: &str) -> Result<PgConnectOptions> {
    let mut url = Url::parse(value).context("invalid DATABASE_URL")?;
    ensure!(
        matches!(url.scheme(), "postgres" | "postgresql"),
        "DATABASE_URL must use postgres:// or postgresql://"
    );

    // Preserve explicit URL settings while retaining a secure default.
    if !url
        .query_pairs()
        .any(|(key, _)| matches!(key.as_ref(), "sslmode" | "ssl-mode"))
    {
        url.query_pairs_mut().append_pair("sslmode", "verify-full");
    }

    let mut options = PgConnectOptions::from_url(&url).context("invalid DATABASE_URL options")?;
    if options.get_application_name().is_none() {
        options = options.application_name("jeeves");
    }

    Ok(options)
}

pub(crate) fn required(name: &str) -> Result<String> {
    optional(name)?.with_context(|| format!("{name} is required; see .env.example"))
}

pub(crate) fn optional(name: &str) -> Result<Option<String>> {
    match env::var(name) {
        Ok(value) if value.trim().is_empty() => Ok(None),
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(error).with_context(|| format!("{name} must contain valid Unicode")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::postgres::PgSslMode;

    #[test]
    fn database_tls_defaults_to_full_verification() {
        for url in [
            "postgres://localhost/jeeves",
            "postgresql://localhost/jeeves?application_name=test",
        ] {
            let options = database_options(url).unwrap();
            assert!(matches!(options.get_ssl_mode(), PgSslMode::VerifyFull));
        }
    }

    #[test]
    fn explicit_url_ssl_modes_are_preserved() {
        for key in ["sslmode", "ssl-mode"] {
            for (value, expected) in [
                ("disable", PgSslMode::Disable),
                ("require", PgSslMode::Require),
                ("verify-ca", PgSslMode::VerifyCa),
                ("verify-full", PgSslMode::VerifyFull),
                ("prefer", PgSslMode::Prefer),
                ("allow", PgSslMode::Allow),
            ] {
                let options =
                    database_options(&format!("postgres://localhost/jeeves?{key}={value}"))
                        .unwrap();
                assert_eq!(
                    std::mem::discriminant(&options.get_ssl_mode()),
                    std::mem::discriminant(&expected)
                );
            }
        }
    }

    #[test]
    fn connection_details_and_ca_path_are_preserved() {
        let options = database_options(
            "postgresql://bot%40example.com:p%40ss@db.example.com:5433/my%20db?sslmode=verify-full&sslrootcert=%2Ftmp%2Fpostgres%20ca.pem&application_name=custom-bot",
        ).unwrap();
        assert_eq!(options.get_username(), "bot@example.com");
        assert_eq!(options.get_host(), "db.example.com");
        assert_eq!(options.get_port(), 5433);
        assert_eq!(options.get_database(), Some("my db"));
        assert_eq!(options.get_application_name(), Some("custom-bot"));
        assert!(
            options
                .to_url_lossy()
                .query_pairs()
                // SQLx's lossy serializer prefixes certificate paths with "file: ".
                .any(|(key, value)| key == "sslrootcert" && value == "file: /tmp/postgres ca.pem")
        );
    }

    #[test]
    fn invalid_urls_and_tls_modes_are_rejected() {
        for url in [
            "not a url",
            "https://localhost/jeeves",
            "postgres://localhost/jeeves?sslmode=verfy-full",
        ] {
            assert!(database_options(url).is_err());
        }
    }
}
