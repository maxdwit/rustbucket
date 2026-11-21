//! Configuration: optional TOML file, overridden by `RUSTBUCKET_*` environment variables.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use ipnet::IpNet;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Address the HTTP server binds to (plain HTTP, TLS is terminated by the nginx next to it, or by your reverse proxy).
    pub listen: SocketAddr,
    /// Public URL of the server, e.g. `https://cloud.example.com`. Takes precedence over
    /// forwarded headers when set.
    pub base_url: Option<String>,
    /// Proxies whose `X-Forwarded-*` headers are trusted.
    pub trusted_proxies: Vec<IpNet>,
    pub database_url: String,
    pub data_dir: PathBuf,
    /// Emit logs as JSON lines.
    pub log_json: bool,
    /// Run pending migrations on `serve`.
    pub auto_migrate: bool,
    /// Largest accepted file in bytes (plain PUT, chunk upload total); 0 = unlimited.
    /// Temporary dev limit: 1 GiB until large-file runs are done on a bigger disk.
    pub max_file_size: u64,
    /// Days a deleted item stays in the trash bin (0 = keep until emptied by hand).
    pub trash_retention_days: u32,
    /// Days an old file version is kept (0 = no age limit).
    pub version_retention_days: u32,
    /// Most old versions kept per file (the oldest go first).
    pub versions_max: u32,
    /// Bearer token for `GET /metrics`; the endpoint does not exist (404) while this is unset.
    pub metrics_token: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([0, 0, 0, 0], 8080)),
            base_url: None,
            trusted_proxies: Vec::new(),
            database_url: "sqlite:///db/rustbucket.db".into(),
            data_dir: PathBuf::from("/data"),
            log_json: true,
            auto_migrate: true,
            max_file_size: 1 << 30,
            trash_retention_days: 30,
            version_retention_days: 90,
            versions_max: 50,
            metrics_token: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config file {0}: {1}")]
    Read(PathBuf, std::io::Error),
    #[error("invalid config file: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid value for {0}: {1}")]
    Env(&'static str, String),
}

impl Config {
    pub fn from_toml(s: &str) -> Result<Self, ConfigError> {
        Ok(toml::from_str(s)?)
    }

    /// Load the optional file, then apply overrides from `lookup` (normally the process env).
    pub fn load(
        file: Option<&Path>,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        let mut cfg = match file {
            Some(p) => Self::from_toml(
                &std::fs::read_to_string(p).map_err(|e| ConfigError::Read(p.into(), e))?,
            )?,
            None => Self::default(),
        };
        cfg.apply_env(lookup)?;
        Ok(cfg)
    }

    pub fn apply_env(
        &mut self,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<(), ConfigError> {
        fn parse<T: std::str::FromStr>(key: &'static str, v: &str) -> Result<T, ConfigError>
        where
            T::Err: std::fmt::Display,
        {
            v.trim()
                .parse()
                .map_err(|e: T::Err| ConfigError::Env(key, e.to_string()))
        }
        if let Some(v) = lookup("RUSTBUCKET_LISTEN") {
            self.listen = parse("RUSTBUCKET_LISTEN", &v)?;
        }
        if let Some(v) = lookup("RUSTBUCKET_BASE_URL") {
            self.base_url =
                Some(v.trim().trim_end_matches('/').to_string()).filter(|s| !s.is_empty());
        }
        if let Some(v) = lookup("RUSTBUCKET_TRUSTED_PROXIES") {
            self.trusted_proxies = v
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| parse("RUSTBUCKET_TRUSTED_PROXIES", s))
                .collect::<Result<_, _>>()?;
        }
        if let Some(v) = lookup("RUSTBUCKET_DATABASE_URL") {
            self.database_url = v;
        }
        if let Some(v) = lookup("RUSTBUCKET_DATA_DIR") {
            self.data_dir = v.into();
        }
        if let Some(v) = lookup("RUSTBUCKET_LOG_JSON") {
            self.log_json = parse("RUSTBUCKET_LOG_JSON", &v)?;
        }
        if let Some(v) = lookup("RUSTBUCKET_AUTO_MIGRATE") {
            self.auto_migrate = parse("RUSTBUCKET_AUTO_MIGRATE", &v)?;
        }
        if let Some(v) = lookup("RUSTBUCKET_TRASH_RETENTION_DAYS") {
            self.trash_retention_days = parse("RUSTBUCKET_TRASH_RETENTION_DAYS", &v)?;
        }
        if let Some(v) = lookup("RUSTBUCKET_VERSION_RETENTION_DAYS") {
            self.version_retention_days = parse("RUSTBUCKET_VERSION_RETENTION_DAYS", &v)?;
        }
        if let Some(v) = lookup("RUSTBUCKET_METRICS_TOKEN") {
            self.metrics_token = Some(v.trim().to_string()).filter(|t| !t.is_empty());
        }
        if let Some(v) = lookup("RUSTBUCKET_VERSIONS_MAX") {
            self.versions_max = parse("RUSTBUCKET_VERSIONS_MAX", &v)?;
        }
        if let Some(v) = lookup("RUSTBUCKET_MAX_FILE_SIZE") {
            self.max_file_size = parse("RUSTBUCKET_MAX_FILE_SIZE", &v)?;
        }
        Ok(())
    }

    /// `true` if a file of `size` bytes may be stored.
    pub fn allows_size(&self, size: u64) -> bool {
        self.max_file_size == 0 || size <= self.max_file_size
    }

    pub fn is_trusted_proxy(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        self.trusted_proxies.iter().any(|n| n.contains(&ip))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn defaults() {
        let c = Config::load(None, env(&[])).unwrap();
        assert_eq!(c.listen.port(), 8080);
        assert!(c.base_url.is_none() && c.trusted_proxies.is_empty());
    }

    #[test]
    fn toml_then_env_override() {
        let mut c = Config::from_toml(
            "base_url = \"https://a.example.com\"\ntrusted_proxies = [\"10.0.0.0/8\"]\nlisten = \"127.0.0.1:9000\"",
        )
        .unwrap();
        assert_eq!(c.listen.port(), 9000);
        c.apply_env(env(&[
            ("RUSTBUCKET_BASE_URL", "https://b.example.com/"),
            ("RUSTBUCKET_TRUSTED_PROXIES", "172.16.0.0/12, ::1/128"),
        ]))
        .unwrap();
        assert_eq!(c.base_url.as_deref(), Some("https://b.example.com"));
        assert_eq!(c.trusted_proxies.len(), 2);
    }

    #[test]
    fn rejects_unknown_keys_and_bad_values() {
        assert!(Config::from_toml("bogus = 1").is_err());
        let mut c = Config::default();
        assert!(
            c.apply_env(env(&[("RUSTBUCKET_TRUSTED_PROXIES", "nope")]))
                .is_err()
        );
        assert!(c.apply_env(env(&[("RUSTBUCKET_LISTEN", "x")])).is_err());
    }

    #[test]
    fn trusted_proxy_matching_incl_mapped_v6() {
        let c = Config {
            trusted_proxies: vec!["172.18.0.0/16".parse().unwrap()],
            ..Config::default()
        };
        assert!(c.is_trusted_proxy("172.18.0.5".parse().unwrap()));
        assert!(c.is_trusted_proxy("::ffff:172.18.0.5".parse().unwrap()));
        assert!(!c.is_trusted_proxy("10.0.0.1".parse().unwrap()));
    }
}
