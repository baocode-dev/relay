//! BaoCode's relay: iroh-relay, admitting devices by key (bans first, then
//! entitlements), one rate limit for all, metrics with no identities in
//! them.

pub mod access;
pub mod config;

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, anyhow};
use iroh_metrics::Registry;
use iroh_relay::server::{
    self as relay, AcmeConfig, CertConfig, QuicConfig, RelayConfig, RelayService, Server,
    ServerConfig, TlsConfig,
};
use tokio::task::JoinHandle;

use crate::{
    access::{Entitlements, Gate, Rules, ids, parse_banned},
    config::{AccessFiles, Config, Limits, Tls},
};

/// A running relay. Stops when dropped.
pub struct Relay {
    server: Server,
    gate: Gate,
    loader: Arc<Loader>,
    _tasks: Vec<Task>,
    _metrics: Option<iroh_metrics::service::MetricsServer>,
}

impl Relay {
    /// Starts the relay. [config_path] is read again now and then for new
    /// limits.
    pub async fn spawn(config: Config, config_path: Option<PathBuf>) -> Result<Self> {
        // For the entitlements URL's client; the relay names its own.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let loader = Arc::new(Loader::new(config.access.clone()));
        let rules = loader.load().context("access rules")?;
        let gate = Gate::new(rules);

        let mut relay_config = RelayConfig::new(config.bind.http);
        relay_config.limits.client_rx = config.limits.rate_limit();
        relay_config.access = Arc::new(gate.clone());
        let mut quic = None;
        if let Some(tls) = &config.tls {
            relay_config.tls = Some(TlsConfig::new(
                config.bind.https,
                certs(&config, tls).await?,
            ));
            if config.quic {
                quic = Some(QuicConfig::new(config.bind.quic));
            }
        }
        let mut server_config = ServerConfig::default();
        server_config.relay = Some(relay_config);
        server_config.quic = quic;
        let server = Server::spawn(server_config)
            .await
            .context("starting the relay")?;
        let service = server
            .relay_service()
            .cloned()
            .context("no relay service")?;

        let metrics = if config.metrics {
            let mut registry = Registry::default();
            registry.register_all(server.metrics());
            registry.register(gate.metrics());
            let server = iroh_metrics::service::MetricsServer::spawn(
                config.bind.metrics,
                Arc::new(registry),
            )
            .await
            .context("starting metrics")?;
            Some(server)
        } else {
            None
        };

        for (tier, limits) in &config.tiers {
            if *limits != config.limits {
                tracing::warn!("tier {tier}: own limits not enforced yet, [limits] applies");
            }
        }

        let mut tasks = vec![Task(tokio::spawn(reload_loop(
            loader.clone(),
            gate.clone(),
            service.clone(),
            config_path,
            config.limits,
        )))];
        if loader.files.entitlements_url.is_some() {
            tasks.push(Task(tokio::spawn(fetch_loop(loader.clone(), gate.clone()))));
        }
        tracing::info!(
            http = ?server.http_addr(),
            https = ?server.https_addr(),
            quic = ?server.quic_addr(),
            "relay up"
        );
        Ok(Self {
            server,
            gate,
            loader,
            _tasks: tasks,
            _metrics: metrics,
        })
    }

    /// Where the relay answers plain HTTP (the relay itself without TLS).
    pub fn http_addr(&self) -> Option<SocketAddr> {
        self.server.http_addr()
    }

    pub fn https_addr(&self) -> Option<SocketAddr> {
        self.server.https_addr()
    }

    pub fn quic_addr(&self) -> Option<SocketAddr> {
        self.server.quic_addr()
    }

    /// Where Prometheus asks, if metrics are on.
    pub fn metrics_addr(&self) -> Option<SocketAddr> {
        self._metrics.as_ref().map(|m| m.local_addr())
    }

    pub fn gate(&self) -> &Gate {
        &self.gate
    }

    pub fn service(&self) -> &RelayService {
        self.server.relay_service().expect("relay service")
    }

    /// Reads the access files now, rather than at the next tick; answers
    /// how many connected devices it shut out.
    pub fn reload(&self) -> Result<usize> {
        apply(&self.loader, &self.gate, self.service())
    }

    /// Asks the entitlements URL now.
    pub async fn fetch(&self) -> Result<()> {
        self.loader.fetch().await
    }

    /// Runs until the server stops on its own.
    pub async fn join(&mut self) {
        let _ = self.server.join().await;
    }

    pub async fn shutdown(self) {
        drop(self._tasks);
        let _ = self.server.shutdown().await;
    }
}

/// A task stopped with the relay.
struct Task(JoinHandle<()>);

impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn certs(config: &Config, tls: &Tls) -> Result<CertConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?
        .with_no_client_auth();
    Ok(match tls {
        Tls::Letsencrypt {
            contact,
            dir,
            staging,
        } => {
            let contact = if contact.starts_with("mailto:") {
                contact.clone()
            } else {
                format!("mailto:{contact}")
            };
            CertConfig::LetsEncrypt {
                acme_config: AcmeConfig::letsencrypt(!staging)
                    .domains(vec![config.hostname.clone()])
                    .contact(vec![contact])
                    .cache_path(dir.clone()),
                server_config_builder: builder,
            }
        }
        Tls::Files { cert, key } => {
            let resolver = relay::reloading_resolver(
                &provider,
                cert.clone(),
                key.clone(),
                Duration::from_secs(3600),
            )
            .await
            .map_err(|e| anyhow!("certificate {}: {e}", cert.display()))?;
            CertConfig::Manual {
                server_config: builder.with_cert_resolver(resolver),
            }
        }
    })
}

/// Builds [Rules] from the access files and what the URL last answered.
pub struct Loader {
    files: AccessFiles,
    /// The URL's last answer, once there is one this run.
    fetched: Mutex<Option<Entitlements>>,
}

impl Loader {
    pub fn new(files: AccessFiles) -> Self {
        Self {
            files,
            fetched: Mutex::new(None),
        }
    }

    pub fn load(&self) -> Result<Rules> {
        let files = &self.files;
        let mut rules = Rules {
            default_tier: Some(files.default_tier.clone()).filter(|t| !t.is_empty()),
            ..Rules::default()
        };
        if let Some(path) = &files.entitlements {
            rules
                .entitled
                .extend(ids(&Entitlements::read(path)?.endpoints));
        }
        let fetched = self
            .fetched
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let remote = match (fetched, &files.cache) {
            (Some(fetched), _) => Some(fetched),
            (None, Some(cache)) if cache.exists() => match Entitlements::read(cache) {
                Ok(cached) => Some(cached),
                Err(e) => {
                    tracing::warn!("entitlements cache unreadable, ignored: {e:#}");
                    None
                }
            },
            _ => None,
        };
        if let Some(remote) = remote {
            rules.entitled.extend(ids(&remote.endpoints));
            rules.banned.extend(ids(&remote.banned));
        }
        if let Some(path) = &files.banned
            && path.exists()
        {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            rules.banned.extend(parse_banned(&text));
        }
        Ok(rules)
    }

    async fn fetch(&self) -> Result<()> {
        let Some(url) = &self.files.entitlements_url else {
            return Ok(());
        };
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()?;
        let mut request = client.get(url);
        if let Some(token) = &self.files.entitlements_token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await?.error_for_status()?;
        let entitlements: Entitlements = serde_json::from_slice(&response.bytes().await?)?;
        if let Some(cache) = &self.files.cache {
            write_atomically(cache, &serde_json::to_vec_pretty(&entitlements)?)?;
        }
        *self.fetched.lock().unwrap_or_else(|e| e.into_inner()) = Some(entitlements);
        Ok(())
    }
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, bytes).with_context(|| format!("writing {}", temp.display()))?;
    std::fs::rename(&temp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Puts the rules the files make now in force, disconnecting whom they
/// shut out.
fn apply(loader: &Loader, gate: &Gate, service: &RelayService) -> Result<usize> {
    let rules = loader.load()?;
    if rules == gate.rules() {
        return Ok(0);
    }
    let out = gate.set_rules(rules);
    for id in &out {
        service.clients().disconnect(*id, None);
    }
    if !out.is_empty() {
        tracing::info!(
            "access rules changed: {} connected device(s) shut out",
            out.len()
        );
    }
    Ok(out.len())
}

async fn reload_loop(
    loader: Arc<Loader>,
    gate: Gate,
    service: RelayService,
    config_path: Option<PathBuf>,
    mut limits: Limits,
) {
    let every = Duration::from_secs(loader.files.reload_secs.max(1));
    let mut last_error = String::new();
    loop {
        tokio::time::sleep(every).await;
        let mut error = String::new();
        if let Err(e) = apply(&loader, &gate, &service) {
            gate.metrics().reload_errors.inc();
            error = format!("{e:#}");
        }
        if let Some(path) = &config_path {
            match Config::read(path) {
                Ok(config) if config.limits != limits => {
                    limits = config.limits;
                    service.set_client_rate_limit(limits.rate_limit());
                    tracing::info!(
                        "rate limit now {} B/s, burst {} B",
                        limits.bytes_per_second,
                        limits.burst_bytes
                    );
                }
                Ok(_) => {}
                Err(e) => error = format!("{e:#}"),
            }
        }
        // Said once, not every tick; the rules before stand meanwhile.
        if error != last_error && !error.is_empty() {
            tracing::error!("reload failed, keeping the rules in force: {error}");
        }
        last_error = error;
    }
}

async fn fetch_loop(loader: Arc<Loader>, gate: Gate) {
    let every = Duration::from_secs(loader.files.refresh_secs.max(10));
    loop {
        if let Err(e) = loader.fetch().await {
            gate.metrics().fetch_errors.inc();
            tracing::warn!("entitlements URL failed, the cache stands: {e:#}");
        }
        tokio::time::sleep(every).await;
    }
}
