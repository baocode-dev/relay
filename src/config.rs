//! The configuration file (TOML). Every field has a default, so `--dev`
//! runs with none.

use std::{
    collections::BTreeMap,
    net::SocketAddr,
    num::NonZeroU32,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use iroh_relay::server::ClientRateLimit;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// The relay's domain, e.g. `relay-hk1.baocode.dev`: what the
    /// certificate is for.
    pub hostname: String,
    pub bind: Bind,
    /// No TLS when absent: only for `--dev` and tests.
    pub tls: Option<Tls>,
    pub limits: Limits,
    /// Limits by entitlement tier. Not enforced one by one yet (iroh-relay
    /// limits every connection the same; see docs/operations.md): until
    /// then `limits` applies to all.
    pub tiers: BTreeMap<String, Limits>,
    pub access: AccessFiles,
    /// QUIC address discovery (UDP), which hole punching needs. Needs TLS.
    pub quic: bool,
    pub metrics: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hostname: String::new(),
            bind: Bind::default(),
            tls: None,
            limits: Limits::default(),
            tiers: BTreeMap::new(),
            access: AccessFiles::default(),
            quic: true,
            metrics: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Bind {
    /// Plain HTTP: ACME challenges and captive portal probes, or the relay
    /// itself without TLS.
    pub http: SocketAddr,
    pub https: SocketAddr,
    pub quic: SocketAddr,
    /// Prometheus; keep it on loopback.
    pub metrics: SocketAddr,
}

impl Default for Bind {
    fn default() -> Self {
        Self {
            http: "[::]:80".parse().unwrap(),
            https: "[::]:443".parse().unwrap(),
            quic: "[::]:7842".parse().unwrap(),
            metrics: "127.0.0.1:9090".parse().unwrap(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase", deny_unknown_fields)]
pub enum Tls {
    /// Let's Encrypt, certificates kept in `dir`.
    Letsencrypt {
        contact: String,
        #[serde(default = "default_cert_dir")]
        dir: PathBuf,
        /// Let's Encrypt's staging directory, for trying a setup out.
        #[serde(default)]
        staging: bool,
    },
    /// PEM files, read again when they change (certbot, a load balancer's
    /// renewals...).
    Files { cert: PathBuf, key: PathBuf },
}

fn default_cert_dir() -> PathBuf {
    PathBuf::from("/var/lib/baocode-relay/certs")
}

/// A token bucket on what each connection sends the relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Sustained bytes a second; 0 is no limit.
    pub bytes_per_second: u32,
    /// Bytes at once over that.
    pub burst_bytes: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            bytes_per_second: 4 << 20,
            burst_bytes: 16 << 20,
        }
    }
}

impl Limits {
    pub fn rate_limit(self) -> Option<ClientRateLimit> {
        let mut limit = ClientRateLimit::new(NonZeroU32::new(self.bytes_per_second)?);
        limit.max_burst_bytes = NonZeroU32::new(self.burst_bytes);
        Some(limit)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccessFiles {
    /// The tier of a device not in the entitlements; empty denies it.
    pub default_tier: String,
    /// `{"endpoints": {"<id>": "<tier>"}}`, read again when it changes.
    pub entitlements: Option<PathBuf>,
    /// Where newer entitlements come from (an account service, later): the
    /// same shape, plus `banned`. Never needed to admit anyone: when it
    /// cannot be reached the last copy fetched, kept in `cache`, stands.
    pub entitlements_url: Option<String>,
    pub entitlements_token: Option<String>,
    pub cache: Option<PathBuf>,
    /// One device id a line, then optionally why; `#` starts a comment.
    pub banned: Option<PathBuf>,
    /// How often files are looked at again, in seconds.
    pub reload_secs: u64,
    /// How often the URL is asked again, in seconds.
    pub refresh_secs: u64,
}

impl Default for AccessFiles {
    fn default() -> Self {
        Self {
            default_tier: "standard".into(),
            entitlements: None,
            entitlements_url: None,
            entitlements_token: None,
            cache: None,
            banned: None,
            reload_secs: 5,
            refresh_secs: 300,
        }
    }
}

impl Config {
    pub fn read(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("in {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let config: Config = toml::from_str(text)?;
        config.check()?;
        Ok(config)
    }

    /// The setup for `--dev`: plain HTTP on [port], everyone admitted.
    pub fn dev(port: u16) -> Self {
        Self {
            bind: Bind {
                http: SocketAddr::from(([127, 0, 0, 1], port)),
                ..Bind::default()
            },
            quic: false,
            metrics: false,
            ..Self::default()
        }
    }

    fn check(&self) -> Result<()> {
        if self.quic && self.tls.is_none() {
            bail!("quic needs tls");
        }
        if let Some(Tls::Letsencrypt { .. }) = self.tls
            && self.hostname.is_empty()
        {
            bail!("letsencrypt needs a hostname");
        }
        if self.limits.bytes_per_second == 0 && self.limits.burst_bytes != 0 {
            bail!("burst_bytes needs bytes_per_second");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_file_reads() {
        let config = Config::parse(
            r#"
            hostname = "relay-hk1.baocode.dev"
            [bind]
            metrics = "127.0.0.1:9100"
            [tls]
            mode = "letsencrypt"
            contact = "ops@baocode.dev"
            [limits]
            bytes_per_second = 1000
            burst_bytes = 2000
            [tiers.pro]
            bytes_per_second = 9000
            [access]
            banned = "/etc/baocode-relay/banned.txt"
            "#,
        )
        .unwrap();
        assert_eq!(config.bind.metrics.port(), 9100);
        assert_eq!(config.bind.https.port(), 443);
        assert_eq!(
            config.limits.rate_limit().unwrap().bytes_per_second.get(),
            1000
        );
        assert_eq!(config.tiers["pro"].bytes_per_second, 9000);
        assert_eq!(config.access.default_tier, "standard");
        assert!(matches!(
            config.tls,
            Some(Tls::Letsencrypt { staging: false, .. })
        ));
    }

    #[test]
    fn no_limit_is_none() {
        let limits = Limits {
            bytes_per_second: 0,
            burst_bytes: 0,
        };
        assert!(limits.rate_limit().is_none());
    }

    #[test]
    fn mistakes_are_told() {
        assert!(Config::parse("quic = true").is_err());
        assert!(Config::parse("[tls]\nmode = \"letsencrypt\"\ncontact = \"a\"").is_err());
        assert!(Config::parse("typo = 1").is_err());
    }
}
