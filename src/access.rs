//! Who may use the relay: the device key, proven by the relay handshake,
//! looked up in the bans, then the entitlements.

use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    str::FromStr,
    sync::{Arc, RwLock},
};

use anyhow::{Context, Result};
use iroh_base::EndpointId;
use iroh_metrics::{Counter, Gauge, MetricsGroup};
use iroh_relay::server::{Access, AccessControl, ClientRequest, ConnectionId};
use serde::{Deserialize, Serialize};

/// What decides admission, swapped whole on every reload.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rules {
    /// Tier of a device not in [entitled]; `None` denies it.
    pub default_tier: Option<String>,
    pub entitled: HashMap<EndpointId, String>,
    /// Device → why.
    pub banned: HashMap<EndpointId, String>,
}

pub enum Decision {
    Allow(String),
    Banned,
    NotEntitled,
}

impl Rules {
    pub fn decide(&self, id: &EndpointId) -> Decision {
        if self.banned.contains_key(id) {
            return Decision::Banned;
        }
        match self.entitled.get(id).or(self.default_tier.as_ref()) {
            Some(tier) => Decision::Allow(tier.clone()),
            None => Decision::NotEntitled,
        }
    }
}

/// The entitlements file, and what the entitlements URL answers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Entitlements {
    /// Device id (hex) → tier.
    pub endpoints: BTreeMap<String, String>,
    /// Device id (hex) → why. Only the URL's answer has these; the local
    /// list is banned.txt.
    pub banned: BTreeMap<String, String>,
}

impl Entitlements {
    pub fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("in {}", path.display()))
    }
}

/// Parses banned.txt: `<device id> [why]` a line, `#` comments.
pub fn parse_banned(text: &str) -> HashMap<EndpointId, String> {
    let mut banned = HashMap::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let mut words = line.splitn(2, char::is_whitespace);
        let Some(id) = words.next().filter(|id| !id.is_empty()) else {
            continue;
        };
        match EndpointId::from_str(id) {
            Ok(id) => {
                let why = words.next().unwrap_or("").trim();
                banned.insert(id, why.to_string());
            }
            Err(_) => tracing::warn!("banned list: not a device id: {id}"),
        }
    }
    banned
}

/// Device ids from [map]'s keys, the ones that parse.
pub fn ids<V: Clone>(map: &BTreeMap<String, V>) -> impl Iterator<Item = (EndpointId, V)> + '_ {
    map.iter().filter_map(|(id, value)| match EndpointId::from_str(id) {
        Ok(id) => Some((id, value.clone())),
        Err(_) => {
            tracing::warn!("entitlements: not a device id: {id}");
            None
        }
    })
}

/// Counts with no identities in them.
#[derive(Debug, Default, MetricsGroup)]
#[metrics(name = "baocode_relay")]
pub struct GateMetrics {
    /// Connections admitted.
    pub admitted: Counter,
    /// Connections refused: device banned.
    pub denied_banned: Counter,
    /// Connections refused: no entitlement.
    pub denied_unentitled: Counter,
    /// Connections dropped by a reload (a ban, an entitlement gone).
    pub kicked: Counter,
    /// Devices connected now.
    pub connected: Gauge,
    /// Devices connected now in the standard tier.
    pub connected_standard: Gauge,
    /// Devices connected now in another tier.
    pub connected_other: Gauge,
    /// Rules reloaded.
    pub reloads: Counter,
    /// Reloads that failed (the rules before them stand).
    pub reload_errors: Counter,
    /// Asks of the entitlements URL that failed (the cache stands).
    pub fetch_errors: Counter,
}

/// The relay's [AccessControl].
#[derive(Debug, Clone)]
pub struct Gate(Arc<GateInner>);

#[derive(Debug)]
struct GateInner {
    rules: RwLock<Rules>,
    /// Who is connected now, and in which tier.
    connected: RwLock<HashMap<EndpointId, Connected>>,
    metrics: Arc<GateMetrics>,
}

#[derive(Debug)]
struct Connected {
    tier: String,
    connections: Vec<ConnectionId>,
}

impl Gate {
    pub fn new(rules: Rules) -> Self {
        Self(Arc::new(GateInner {
            rules: RwLock::new(rules),
            connected: RwLock::new(HashMap::new()),
            metrics: Arc::default(),
        }))
    }

    pub fn metrics(&self) -> Arc<GateMetrics> {
        self.0.metrics.clone()
    }

    pub fn rules(&self) -> Rules {
        self.0.rules.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Puts [rules] in force; answers the connected devices they no longer
    /// admit, for the caller to disconnect.
    pub fn set_rules(&self, rules: Rules) -> Vec<EndpointId> {
        // Rules, then connected, as in on_connect: no one admitted by the
        // old rules goes unseen by the new.
        let mut current = self.0.rules.write().unwrap_or_else(|e| e.into_inner());
        let out: Vec<EndpointId> = {
            let connected = self.0.connected.read().unwrap_or_else(|e| e.into_inner());
            connected
                .keys()
                .filter(|id| !matches!(rules.decide(id), Decision::Allow(_)))
                .copied()
                .collect()
        };
        *current = rules;
        drop(current);
        self.0.metrics.reloads.inc();
        self.0.metrics.kicked.inc_by(out.len() as u64);
        out
    }

    pub fn connected(&self) -> usize {
        self.0.connected.read().unwrap_or_else(|e| e.into_inner()).len()
    }

    fn count(&self, connected: &HashMap<EndpointId, Connected>) {
        let standard = connected.values().filter(|c| c.tier == "standard").count();
        let metrics = &self.0.metrics;
        metrics.connected.set(connected.len() as i64);
        metrics.connected_standard.set(standard as i64);
        metrics.connected_other.set((connected.len() - standard) as i64);
    }
}

impl AccessControl for Gate {
    async fn on_connect(&self, request: &ClientRequest) -> Access {
        let id = request.endpoint_id();
        let rules = self.0.rules.read().unwrap_or_else(|e| e.into_inner());
        match rules.decide(&id) {
            Decision::Allow(tier) => {
                self.0.metrics.admitted.inc();
                let mut connected = self.0.connected.write().unwrap_or_else(|e| e.into_inner());
                let entry = connected.entry(id).or_insert_with(|| Connected {
                    tier: tier.clone(),
                    connections: Vec::new(),
                });
                entry.tier = tier;
                entry.connections.push(request.connection_id());
                self.count(&connected);
                Access::Allow
            }
            Decision::Banned => {
                self.0.metrics.denied_banned.inc();
                Access::Deny {
                    reason: Some("banned".into()),
                }
            }
            Decision::NotEntitled => {
                self.0.metrics.denied_unentitled.inc();
                Access::Deny {
                    reason: Some("not entitled".into()),
                }
            }
        }
    }

    fn on_disconnect(&self, id: EndpointId, connection: ConnectionId) {
        let mut connected = self.0.connected.write().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = connected.get_mut(&id) {
            entry.connections.retain(|c| *c != connection);
            if entry.connections.is_empty() {
                connected.remove(&id);
            }
        }
        self.count(&connected);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> EndpointId {
        iroh_base::SecretKey::from_bytes(&[n; 32]).public()
    }

    #[test]
    fn bans_come_before_entitlements() {
        let mut rules = Rules {
            default_tier: Some("standard".into()),
            ..Rules::default()
        };
        rules.entitled.insert(id(1), "pro".into());
        rules.banned.insert(id(1), "spam".into());
        assert!(matches!(rules.decide(&id(1)), Decision::Banned));
        assert!(matches!(rules.decide(&id(2)), Decision::Allow(t) if t == "standard"));
        rules.default_tier = None;
        assert!(matches!(rules.decide(&id(2)), Decision::NotEntitled));
    }

    #[test]
    fn the_banned_list_reads() {
        let text = format!(
            "# who\n{} spam and more\n\n  {}  \nnot-an-id\n",
            id(1),
            id(2)
        );
        let banned = parse_banned(&text);
        assert_eq!(banned.len(), 2);
        assert_eq!(banned[&id(1)], "spam and more");
        assert_eq!(banned[&id(2)], "");
    }

    #[test]
    fn new_rules_name_whom_they_shut_out() {
        let gate = Gate::new(Rules {
            default_tier: Some("standard".into()),
            ..Rules::default()
        });
        gate.0.connected.write().unwrap().insert(
            id(1),
            Connected {
                tier: "standard".into(),
                connections: vec![],
            },
        );
        let mut rules = gate.rules();
        rules.banned.insert(id(1), String::new());
        assert_eq!(gate.set_rules(rules), vec![id(1)]);
        assert_eq!(gate.metrics().kicked.get(), 1);
    }
}
