//! Validated configuration for the Rust/PostgreSQL Cartograph runtime.
//!
//! The database URL is secret-bearing input. It stays wrapped in
//! [`secrecy::SecretString`] after validation and is never included in an error
//! or debug representation.

use std::{env, num::NonZeroU32, time::Duration};

use secrecy::SecretString;
use thiserror::Error;
use url::Url;

mod numeric;
mod project;

pub use numeric::{BoundedU64Field, optional_bounded_u64};

pub use project::{
    ProjectConfigError, ProjectConfigSnapshot, ProjectGenerationStorage, ProjectSourceSettings,
    load_project_source_settings, read_project_config, read_project_config_snapshot,
    update_project_config, write_project_config_if_unchanged, write_project_max_file_size,
};

/// Environment variable containing the only supported v2 database URL.
pub const DATABASE_URL_ENV: &str = "CARTOGRAPH_DATABASE_URL";
/// Environment variable controlling the bounded PostgreSQL connection pool.
pub const DATABASE_MAX_CONNECTIONS_ENV: &str = "CARTOGRAPH_DATABASE_MAX_CONNECTIONS";
/// Environment variable controlling pool acquisition timeout in milliseconds.
pub const DATABASE_ACQUIRE_TIMEOUT_MS_ENV: &str = "CARTOGRAPH_DATABASE_ACQUIRE_TIMEOUT_MS";
/// Environment variable setting the connection's default PostgreSQL statement timeout.
pub const DATABASE_QUERY_TIMEOUT_MS_ENV: &str = "CARTOGRAPH_DATABASE_QUERY_TIMEOUT_MS";
/// Environment variable forcing PostgreSQL TLS when set to `true`.
pub const DATABASE_REQUIRE_SSL_ENV: &str = "CARTOGRAPH_DATABASE_REQUIRE_SSL";
/// Environment variable selecting the isolated PostgreSQL schema.
pub const DATABASE_SCHEMA_ENV: &str = "CARTOGRAPH_DATABASE_SCHEMA";

const DEFAULT_MAX_CONNECTIONS: u32 = 8;
const MAX_CONNECTIONS_LIMIT: u32 = 64;
const DEFAULT_ACQUIRE_TIMEOUT_MS: u64 = 5_000;
const MAX_ACQUIRE_TIMEOUT_MS: u64 = 120_000;
const DEFAULT_QUERY_TIMEOUT_MS: u64 = 120_000;
const MAX_QUERY_TIMEOUT_MS: u64 = 600_000;
const DEFAULT_DATABASE_SCHEMA: &str = "cartograph";
const POSTGRES_IDENTIFIER_LENGTH_LIMIT: usize = 63;

struct BoundedIntegerInput<'a, T> {
    key: &'static str,
    raw: Option<&'a str>,
    default: T,
    maximum: T,
}

/// Validated Postgres-only database settings.
#[derive(Clone)]
pub struct DatabaseSettings {
    url: SecretString,
    max_connections: NonZeroU32,
    acquire_timeout: Duration,
    query_timeout: Duration,
    require_ssl: bool,
    schema: DatabaseSchema,
}

impl std::fmt::Debug for DatabaseSettings {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DatabaseSettings")
            .field("url", &"[REDACTED]")
            .field("max_connections", &self.max_connections)
            .field("acquire_timeout", &self.acquire_timeout)
            .field("query_timeout", &self.query_timeout)
            .field("require_ssl", &self.require_ssl)
            .field("schema", &self.schema)
            .finish()
    }
}

/// Validated, canonical PostgreSQL identifier used as Cartograph's schema.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DatabaseSchema(String);

impl DatabaseSchema {
    /// Load the optional process setting, falling back to Cartograph's default
    /// schema while applying the same identifier validation as config files.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidDatabaseSchema`] when the environment
    /// value is non-Unicode or is not a valid unquoted PostgreSQL identifier.
    pub fn from_env() -> Result<Self, ConfigError> {
        match env::var(DATABASE_SCHEMA_ENV) {
            Ok(schema) => Self::parse(&schema),
            Err(env::VarError::NotPresent) => Ok(Self(DEFAULT_DATABASE_SCHEMA.to_owned())),
            Err(env::VarError::NotUnicode(_)) => Err(ConfigError::InvalidDatabaseSchema),
        }
    }

    /// Parse a conservative unquoted PostgreSQL identifier and normalize it to
    /// lowercase before the database layer quotes it.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidDatabaseSchema`] when `raw` is empty,
    /// non-ASCII, too long, or contains invalid identifier characters.
    pub fn parse(raw: &str) -> Result<Self, ConfigError> {
        let mut bytes = raw.bytes();
        let valid_first = bytes
            .next()
            .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic());
        let valid_rest = bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric());
        if raw.len() > POSTGRES_IDENTIFIER_LENGTH_LIMIT
            || !raw.is_ascii()
            || !valid_first
            || !valid_rest
        {
            return Err(ConfigError::InvalidDatabaseSchema);
        }
        Ok(Self(raw.to_ascii_lowercase()))
    }

    /// Canonical identifier text. Database code must still quote it as an SQL
    /// identifier rather than interpolating it as a bare name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl DatabaseSettings {
    /// Load settings from the process environment and validate every value.
    ///
    /// # Errors
    ///
    /// Returns a [`ConfigError`] when a required setting is absent or any
    /// supplied database, bound, timeout, TLS, or schema value is invalid.
    pub fn from_env() -> Result<Self, ConfigError> {
        let database_url =
            env::var(DATABASE_URL_ENV).map_err(|_| ConfigError::MissingDatabaseUrl)?;
        let max_connections = env::var(DATABASE_MAX_CONNECTIONS_ENV).ok();
        let acquire_timeout_ms = env::var(DATABASE_ACQUIRE_TIMEOUT_MS_ENV).ok();
        let query_timeout_ms = env::var(DATABASE_QUERY_TIMEOUT_MS_ENV).ok();
        let require_ssl = parse_optional_bool_env(DATABASE_REQUIRE_SSL_ENV)?;
        let schema = DatabaseSchema::from_env()?;
        Self::parse(
            &database_url,
            max_connections.as_deref(),
            acquire_timeout_ms.as_deref(),
        )?
        .with_query_timeout_ms(query_timeout_ms.as_deref())?
        .with_require_ssl(require_ssl)
        .with_schema(schema.as_str())
    }

    /// Validate settings supplied by a non-environment boundary, such as a
    /// future project config loader.
    ///
    /// # Errors
    ///
    /// Returns a [`ConfigError`] when the database URL is not an accepted
    /// PostgreSQL URL or either numeric setting is outside its hard bound.
    pub fn parse(
        database_url: &str,
        max_connections: Option<&str>,
        acquire_timeout_ms: Option<&str>,
    ) -> Result<Self, ConfigError> {
        validate_database_url(database_url)?;
        let max_connections = parse_bounded_nonzero_u32(&BoundedIntegerInput {
            key: DATABASE_MAX_CONNECTIONS_ENV,
            raw: max_connections,
            default: DEFAULT_MAX_CONNECTIONS,
            maximum: MAX_CONNECTIONS_LIMIT,
        })?;
        let acquire_timeout_ms = parse_bounded_nonzero_u64(&BoundedIntegerInput {
            key: DATABASE_ACQUIRE_TIMEOUT_MS_ENV,
            raw: acquire_timeout_ms,
            default: DEFAULT_ACQUIRE_TIMEOUT_MS,
            maximum: MAX_ACQUIRE_TIMEOUT_MS,
        })?;

        Ok(Self {
            url: SecretString::from(database_url.to_owned()),
            max_connections,
            acquire_timeout: Duration::from_millis(acquire_timeout_ms),
            query_timeout: Duration::from_millis(DEFAULT_QUERY_TIMEOUT_MS),
            require_ssl: false,
            schema: DatabaseSchema(DEFAULT_DATABASE_SCHEMA.to_owned()),
        })
    }

    /// Replace the connection-pool cap using the same hard bound as environment input.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidBoundedInteger`] when `value` is zero or
    /// exceeds the supported connection limit.
    pub fn with_max_connections(mut self, value: u32) -> Result<Self, ConfigError> {
        self.max_connections = NonZeroU32::new(value)
            .filter(|value| value.get() <= MAX_CONNECTIONS_LIMIT)
            .ok_or(ConfigError::InvalidBoundedInteger {
                key: DATABASE_MAX_CONNECTIONS_ENV,
                minimum: 1,
                maximum: u64::from(MAX_CONNECTIONS_LIMIT),
            })?;
        Ok(self)
    }

    /// Replace the pool/initial-connect acquisition timeout in milliseconds.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidBoundedInteger`] when `value` is zero or
    /// exceeds the supported acquisition timeout.
    pub fn with_acquire_timeout_ms(mut self, value: u64) -> Result<Self, ConfigError> {
        let value = parse_bounded_nonzero_u64(&BoundedIntegerInput {
            key: DATABASE_ACQUIRE_TIMEOUT_MS_ENV,
            raw: Some(&value.to_string()),
            default: DEFAULT_ACQUIRE_TIMEOUT_MS,
            maximum: MAX_ACQUIRE_TIMEOUT_MS,
        })?;
        self.acquire_timeout = Duration::from_millis(value);
        Ok(self)
    }

    /// Replace the PostgreSQL session statement timeout in milliseconds.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidBoundedInteger`] when `raw` is malformed,
    /// zero, or exceeds the supported query timeout.
    pub fn with_query_timeout_ms(mut self, raw: Option<&str>) -> Result<Self, ConfigError> {
        let value = parse_bounded_nonzero_u64(&BoundedIntegerInput {
            key: DATABASE_QUERY_TIMEOUT_MS_ENV,
            raw,
            default: DEFAULT_QUERY_TIMEOUT_MS,
            maximum: MAX_QUERY_TIMEOUT_MS,
        })?;
        self.query_timeout = Duration::from_millis(value);
        Ok(self)
    }

    /// Force TLS at the driver boundary even when the URL omits `sslmode=require`.
    #[must_use]
    pub const fn with_require_ssl(mut self, require_ssl: bool) -> Self {
        self.require_ssl = require_ssl;
        self
    }

    /// Replace the default schema with a validated project/deployment schema.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidDatabaseSchema`] when `raw` is not a
    /// bounded unquoted PostgreSQL identifier.
    pub fn with_schema(mut self, raw: &str) -> Result<Self, ConfigError> {
        self.schema = DatabaseSchema::parse(raw)?;
        Ok(self)
    }

    /// The validated secret-bearing connection URL.
    #[must_use]
    pub const fn url(&self) -> &SecretString {
        &self.url
    }

    /// Maximum number of concurrent PostgreSQL connections.
    #[must_use]
    pub const fn max_connections(&self) -> NonZeroU32 {
        self.max_connections
    }

    /// Maximum time to wait for a pooled connection.
    #[must_use]
    pub const fn acquire_timeout(&self) -> Duration {
        self.acquire_timeout
    }

    /// Default PostgreSQL statement timeout installed on every pooled connection.
    #[must_use]
    pub const fn query_timeout(&self) -> Duration {
        self.query_timeout
    }

    /// Whether the driver must force TLS independently of URL parameters.
    #[must_use]
    pub const fn require_ssl(&self) -> bool {
        self.require_ssl
    }

    /// Isolated PostgreSQL schema used by Cartograph.
    #[must_use]
    pub const fn schema(&self) -> &DatabaseSchema {
        &self.schema
    }
}

/// Configuration failures whose messages never echo secret-bearing input.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    /// No v2 database URL was supplied.
    #[error("{DATABASE_URL_ENV} is required; Cartograph v2 supports PostgreSQL only")]
    MissingDatabaseUrl,
    /// The URL could not be parsed.
    #[error("{DATABASE_URL_ENV} is not a valid URL")]
    InvalidDatabaseUrl,
    /// A non-PostgreSQL scheme was supplied.
    #[error("{DATABASE_URL_ENV} must use the postgres or postgresql scheme")]
    UnsupportedDatabaseScheme,
    /// A network host is required in the first v2 runtime slice.
    #[error("{DATABASE_URL_ENV} must include a database host")]
    MissingDatabaseHost,
    /// A bounded numeric setting was malformed or outside its range.
    #[error("{key} must be an integer in the inclusive range {minimum}..={maximum}")]
    InvalidBoundedInteger {
        /// Environment/config key whose value failed validation.
        key: &'static str,
        /// Smallest accepted value.
        minimum: u64,
        /// Largest accepted value.
        maximum: u64,
    },
    /// The schema could alter SQL structure or exceed PostgreSQL's identifier limit.
    #[error(
        "{DATABASE_SCHEMA_ENV} must be a 1..=63 byte ASCII identifier beginning with a letter or underscore"
    )]
    InvalidDatabaseSchema,
    /// A boolean environment setting was neither `true` nor `false`.
    #[error("{key} must be true or false")]
    InvalidBoolean {
        /// Environment key whose value was malformed.
        key: &'static str,
    },
}

fn parse_optional_bool_env(key: &'static str) -> Result<bool, ConfigError> {
    match env::var(key) {
        Ok(value) if value.eq_ignore_ascii_case("true") || value == "1" => Ok(true),
        Ok(value) if value.eq_ignore_ascii_case("false") || value == "0" => Ok(false),
        Ok(_) | Err(env::VarError::NotUnicode(_)) => Err(ConfigError::InvalidBoolean { key }),
        Err(env::VarError::NotPresent) => Ok(false),
    }
}

fn validate_database_url(database_url: &str) -> Result<(), ConfigError> {
    let parsed = Url::parse(database_url).map_err(|_| ConfigError::InvalidDatabaseUrl)?;
    if !matches!(parsed.scheme(), "postgres" | "postgresql") {
        return Err(ConfigError::UnsupportedDatabaseScheme);
    }
    if parsed.host_str().is_none() {
        return Err(ConfigError::MissingDatabaseHost);
    }
    Ok(())
}

fn parse_bounded_nonzero_u32(
    input: &BoundedIntegerInput<'_, u32>,
) -> Result<NonZeroU32, ConfigError> {
    let value = match input.raw {
        Some(raw) => raw.parse::<u32>().ok(),
        None => Some(input.default),
    }
    .filter(|value| *value <= input.maximum)
    .and_then(NonZeroU32::new)
    .ok_or(ConfigError::InvalidBoundedInteger {
        key: input.key,
        minimum: 1,
        maximum: u64::from(input.maximum),
    })?;
    Ok(value)
}

fn parse_bounded_nonzero_u64(input: &BoundedIntegerInput<'_, u64>) -> Result<u64, ConfigError> {
    match input.raw {
        Some(raw) => raw.parse::<u64>().ok(),
        None => Some(input.default),
    }
    .filter(|value| (1..=input.maximum).contains(value))
    .ok_or(ConfigError::InvalidBoundedInteger {
        key: input.key,
        minimum: 1,
        maximum: input.maximum,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    const VALID_URL: &str = "postgresql://cartograph:secret@localhost:5433/cartograph";

    #[test]
    fn accepts_postgres_and_applies_bounded_defaults() {
        let settings = DatabaseSettings::parse(VALID_URL, None, None);
        let settings = match settings {
            Ok(settings) => settings,
            Err(error) => panic!("valid settings unexpectedly failed: {error}"),
        };

        assert_eq!(settings.url().expose_secret(), VALID_URL);
        assert_eq!(settings.max_connections().get(), DEFAULT_MAX_CONNECTIONS);
        assert_eq!(
            settings.acquire_timeout(),
            Duration::from_millis(DEFAULT_ACQUIRE_TIMEOUT_MS)
        );
    }

    #[test]
    fn rejects_sqlite_without_echoing_the_secret_input() {
        let input = "sqlite:///tmp/private-cartograph.db?password=should-not-leak";
        let error = DatabaseSettings::parse(input, None, None).err();
        let Some(error) = error else {
            panic!("SQLite URL was accepted");
        };

        assert_eq!(error, ConfigError::UnsupportedDatabaseScheme);
        assert!(!error.to_string().contains("should-not-leak"));
        assert!(!format!("{error:?}").contains("should-not-leak"));
    }

    #[test]
    fn debug_output_redacts_database_credentials() {
        let settings = DatabaseSettings::parse(VALID_URL, Some("3"), Some("7000"));
        let settings = match settings {
            Ok(settings) => settings,
            Err(error) => panic!("valid settings unexpectedly failed: {error}"),
        };
        let rendered = format!("{settings:?}");

        assert!(rendered.contains("[REDACTED]"));
        assert!(!rendered.contains("secret"));
        assert!(!rendered.contains("localhost"));
    }

    #[test]
    fn rejects_zero_and_excessive_pool_limits() {
        for invalid in ["0", "65", "not-a-number"] {
            let error = DatabaseSettings::parse(VALID_URL, Some(invalid), None).err();
            assert!(matches!(
                error,
                Some(ConfigError::InvalidBoundedInteger {
                    key: DATABASE_MAX_CONNECTIONS_ENV,
                    minimum: 1,
                    maximum: 64,
                })
            ));
        }
    }

    #[test]
    fn rejects_url_without_network_host() {
        assert_eq!(
            DatabaseSettings::parse("postgresql:///cartograph", None, None).err(),
            Some(ConfigError::MissingDatabaseHost)
        );
    }

    #[test]
    fn schema_names_are_canonical_and_cannot_change_sql_structure() {
        let schema = DatabaseSchema::parse("Team_Cartograph");
        assert!(matches!(schema, Ok(value) if value.as_str() == "team_cartograph"));

        for invalid in [
            "",
            "2cartograph",
            "cartograph-dev",
            "cartograph.public",
            "cartograph\";DROP SCHEMA public;--",
            "café",
        ] {
            assert!(
                DatabaseSchema::parse(invalid).is_err(),
                "accepted {invalid:?}"
            );
        }
        assert!(DatabaseSchema::parse(&"a".repeat(64)).is_err());
    }

    #[test]
    fn database_settings_default_to_the_cartograph_schema() {
        let settings = DatabaseSettings::parse(VALID_URL, None, None);
        let settings = match settings {
            Ok(settings) => settings,
            Err(error) => panic!("valid settings unexpectedly failed: {error}"),
        };
        assert_eq!(settings.schema().as_str(), "cartograph");

        let configured = settings.with_schema("Review_Worktree");
        assert!(matches!(configured, Ok(value) if value.schema().as_str() == "review_worktree"));
    }
}
