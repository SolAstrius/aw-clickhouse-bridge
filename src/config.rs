use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub clickhouse: ClickHouseConfig,
    #[serde(default)]
    pub server: ServerConfig,
}

#[derive(Debug, Default, Deserialize)]
pub struct ClickHouseConfig {
    pub url: Option<String>,
    pub database: Option<String>,
    pub user: Option<String>,
    pub password: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ServerConfig {
    pub port: Option<u16>,
    pub bind_addr: Option<String>,
}

impl Config {
    /// Load config from file, then apply env var overrides
    pub fn load() -> Self {
        let mut config = Self::load_from_file().unwrap_or_default();
        config.apply_env_overrides();
        config
    }

    fn load_from_file() -> Option<Self> {
        let config_path = Self::config_path()?;
        if !config_path.exists() {
            return None;
        }

        let content = std::fs::read_to_string(&config_path).ok()?;
        match toml::from_str(&content) {
            Ok(config) => {
                tracing::info!("Loaded config from {}", config_path.display());
                Some(config)
            }
            Err(e) => {
                tracing::warn!("Failed to parse {}: {}", config_path.display(), e);
                None
            }
        }
    }

    fn apply_env_overrides(&mut self) {
        if let Ok(v) = std::env::var("CLICKHOUSE_URL") {
            self.clickhouse.url = Some(v);
        }
        if let Ok(v) = std::env::var("CLICKHOUSE_DATABASE") {
            self.clickhouse.database = Some(v);
        }
        if let Ok(v) = std::env::var("CLICKHOUSE_USER") {
            self.clickhouse.user = Some(v);
        }
        if let Ok(v) = std::env::var("CLICKHOUSE_PASSWORD") {
            self.clickhouse.password = Some(v);
        }
        if let Ok(v) = std::env::var("PORT") {
            if let Ok(port) = v.parse() {
                self.server.port = Some(port);
            }
        }
        if let Ok(v) = std::env::var("BIND_ADDR") {
            self.server.bind_addr = Some(v);
        }
    }

    pub fn config_path() -> Option<PathBuf> {
        dirs::config_dir().map(|p| p.join("aw-clickhouse-bridge").join("config.toml"))
    }

    // Accessors with defaults
    pub fn clickhouse_url(&self) -> String {
        self.clickhouse.url.clone().unwrap_or_else(|| "http://localhost:8123".into())
    }

    pub fn clickhouse_database(&self) -> String {
        self.clickhouse.database.clone().unwrap_or_else(|| "activitywatch".into())
    }

    pub fn clickhouse_user(&self) -> Option<&str> {
        self.clickhouse.user.as_deref()
    }

    pub fn clickhouse_password(&self) -> Option<&str> {
        self.clickhouse.password.as_deref()
    }

    pub fn port(&self) -> u16 {
        self.server.port.unwrap_or(5600)
    }

    pub fn bind_addrs(&self) -> Vec<String> {
        let default_port = self.port();

        let addrs: Vec<String> = self.server.bind_addr
            .as_ref()
            .map(|s| s.split(',').map(|a| a.trim().to_string()).collect())
            .unwrap_or_else(|| vec![format!("127.0.0.1:{}", default_port)]);

        addrs
            .into_iter()
            .map(|addr| {
                if addr.contains(':') {
                    addr
                } else {
                    format!("{}:{}", addr, default_port)
                }
            })
            .collect()
    }
}
