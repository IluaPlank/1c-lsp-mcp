use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::fs;

use crate::paths::AppPaths;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub jar_path: String,
    #[serde(default = "default_listen_host")]
    pub listen_host: String,
    #[serde(default = "default_http_port")]
    pub http_port: u16,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Единый MCP endpoint (диагностика + навигация + дальнейшие tools).
    #[serde(default)]
    pub mcp_enabled: bool,
    #[serde(default = "default_mcp_port")]
    pub mcp_port: u16,
    /// Legacy: раньше отдельный diagnostics MCP. Читается при миграции.
    #[serde(default)]
    pub mcp_diagnostics_enabled: bool,
    #[serde(default = "default_mcp_diagnostics_port")]
    pub mcp_diagnostics_port: u16,
    /// Legacy: раньше отдельный navigation MCP. Читается при миграции.
    #[serde(default)]
    pub mcp_navigation_enabled: bool,
    #[serde(default = "default_mcp_navigation_port")]
    pub mcp_navigation_port: u16,
    #[serde(default = "default_use_toon_format")]
    pub use_toon_format: bool,
    /// Путь к каталогу платформы 1С (для bsl-context).
    #[serde(default)]
    pub platform_path: String,
    /// Путь к mcp-bsl-context-*.jar. Пусто = выключено.
    #[serde(default)]
    pub bsl_context_jar_path: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            jar_path: String::new(),
            listen_host: default_listen_host(),
            http_port: default_http_port(),
            log_level: default_log_level(),
            mcp_enabled: false,
            mcp_port: default_mcp_port(),
            mcp_diagnostics_enabled: false,
            mcp_diagnostics_port: default_mcp_diagnostics_port(),
            mcp_navigation_enabled: false,
            mcp_navigation_port: default_mcp_navigation_port(),
            use_toon_format: default_use_toon_format(),
            platform_path: String::new(),
            bsl_context_jar_path: String::new(),
        }
    }
}

impl AppConfig {
    pub async fn load_or_create(paths: &AppPaths) -> Result<Self> {
        match fs::read_to_string(&paths.config_path).await {
            Ok(content) => {
                let mut config: Self =
                    toml::from_str(&content).context("failed to parse config.toml")?;
                config.migrate_legacy_mcp();
                Ok(config)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let config = Self::default();
                config.save(paths).await?;
                Ok(config)
            }
            Err(err) => Err(err).context("failed to read config.toml"),
        }
    }

    pub async fn save(&self, paths: &AppPaths) -> Result<()> {
        let body = toml::to_string_pretty(self).context("failed to serialize config")?;
        fs::write(&paths.config_path, body)
            .await
            .context("failed to write config.toml")?;
        Ok(())
    }

    /// Сводит старые два MCP-флага в один endpoint.
    pub fn migrate_legacy_mcp(&mut self) {
        if self.mcp_enabled {
            return;
        }
        if self.mcp_diagnostics_enabled || self.mcp_navigation_enabled {
            self.mcp_enabled = true;
            self.mcp_port = if self.mcp_diagnostics_enabled {
                self.mcp_diagnostics_port
            } else {
                self.mcp_navigation_port
            };
        }
    }

    /// Порт единого MCP, если он включён (после миграции legacy).
    pub fn effective_mcp_port(&self) -> Option<u16> {
        let mut cfg = self.clone();
        cfg.migrate_legacy_mcp();
        if cfg.mcp_enabled {
            Some(cfg.mcp_port)
        } else {
            None
        }
    }

    pub fn bsl_context_configured(&self) -> bool {
        !self.platform_path.trim().is_empty() && !self.bsl_context_jar_path.trim().is_empty()
    }
}

fn default_listen_host() -> String {
    "0.0.0.0".to_string()
}

fn default_http_port() -> u16 {
    4000
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_use_toon_format() -> bool {
    true
}

fn default_mcp_port() -> u16 {
    9010
}

fn default_mcp_diagnostics_port() -> u16 {
    9011
}

fn default_mcp_navigation_port() -> u16 {
    9012
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_legacy_diagnostics_flag() {
        let mut cfg = AppConfig {
            mcp_diagnostics_enabled: true,
            mcp_diagnostics_port: 9011,
            ..AppConfig::default()
        };
        cfg.migrate_legacy_mcp();
        assert!(cfg.mcp_enabled);
        assert_eq!(cfg.mcp_port, 9011);
        assert_eq!(cfg.effective_mcp_port(), Some(9011));
    }

    #[test]
    fn prefers_explicit_unified_flag() {
        let cfg = AppConfig {
            mcp_enabled: true,
            mcp_port: 9010,
            mcp_diagnostics_enabled: true,
            mcp_diagnostics_port: 9011,
            ..AppConfig::default()
        };
        assert_eq!(cfg.effective_mcp_port(), Some(9010));
    }

    #[test]
    fn toon_is_the_default_for_agents() {
        assert!(AppConfig::default().use_toon_format);
        let parsed: AppConfig = toml::from_str("").unwrap();
        assert!(parsed.use_toon_format);
        let disabled: AppConfig = toml::from_str("use_toon_format = false").unwrap();
        assert!(!disabled.use_toon_format);
    }

    #[test]
    fn disabled_when_nothing_enabled() {
        assert_eq!(AppConfig::default().effective_mcp_port(), None);
    }
}
