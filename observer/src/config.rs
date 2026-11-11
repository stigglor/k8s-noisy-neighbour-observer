//! Process config from `NNO_*` environment variables.

use std::net::SocketAddr;

use anyhow::Context;

const DEFAULT_BIND_ADDR: &str = "[::]:9400";

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: SocketAddr,
}

impl Config {
    /// Fails on an invalid value, so a typo stops the pod at startup.
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let raw = get("NNO_BIND_ADDR").unwrap_or_else(|| DEFAULT_BIND_ADDR.to_string());
        let bind_addr = raw
            .parse()
            .with_context(|| format!("NNO_BIND_ADDR is not a socket address: {raw}"))?;
        Ok(Self { bind_addr })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_binds_ipv6_any() {
        let config = Config::from_lookup(|_| None).unwrap();
        assert_eq!(config.bind_addr.to_string(), "[::]:9400");
    }

    #[test]
    fn env_overrides_default() {
        let config = Config::from_lookup(|_| Some("127.0.0.1:8080".into())).unwrap();
        assert_eq!(config.bind_addr.port(), 8080);
    }

    #[test]
    fn invalid_value_fails() {
        assert!(Config::from_lookup(|_| Some("not-an-address".into())).is_err());
    }
}
