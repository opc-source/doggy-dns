//! Connection pooling for Nacos watchers across config reloads.
//!
//! A `NacosServiceWatcher` owns a gRPC connection to the naming service and
//! the instance cache filled by its push subscriptions. Rebuilding one is the
//! expensive part of rebuilding an authority chain, so a reload must not do it
//! unless the connection itself changed.
//!
//! [`plan_reuse`] is the pure half of that decision: given the targets the new
//! config asks for and the identities already pooled, it says which pooled
//! watcher serves which target. [`PooledWatcher`] is the effecting half.

use crate::watcher::{CachedInstance, NacosServiceWatcher, ServiceKey};
use anyhow::Result;
use dashmap::DashMap;
use doggy_dns_basic::PluginConfig;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;

/// Values applied when a nacos plugin leaves the connection fields unset.
const DEFAULT_SERVER_ADDR: &str = "127.0.0.1:8848";
const DEFAULT_NAMESPACE: &str = "public";
const DEFAULT_GROUP: &str = "DEFAULT_GROUP";
const DEFAULT_ZONE: &str = "nacos.local";

/// What identifies one naming connection.
///
/// Zone and TTL are deliberately absent: they shape the DNS answers built on
/// top of a cache, not the connection feeding it, so changing a zone keeps the
/// connection (and everything already pushed into the cache).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NacosIdentity {
    pub server_addr: String,
    pub namespace: String,
    pub group: String,
}

/// A nacos plugin resolved to concrete values, defaults applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NacosTarget {
    pub identity: NacosIdentity,
    pub zone: String,
    pub ttl: u32,
}

impl NacosTarget {
    /// Read the nacos fields of a plugin config, applying the same defaults
    /// the authority has always applied.
    pub fn from_plugin(plugin: &PluginConfig) -> Self {
        Self {
            identity: NacosIdentity {
                server_addr: plugin
                    .server_addr
                    .clone()
                    .unwrap_or_else(|| DEFAULT_SERVER_ADDR.to_string()),
                namespace: plugin
                    .namespace
                    .clone()
                    .unwrap_or_else(|| DEFAULT_NAMESPACE.to_string()),
                group: plugin
                    .group
                    .clone()
                    .unwrap_or_else(|| DEFAULT_GROUP.to_string()),
            },
            zone: plugin
                .dns_zone
                .clone()
                .unwrap_or_else(|| DEFAULT_ZONE.to_string()),
            ttl: plugin.ttl,
        }
    }
}

/// What the pool can do for one target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReusePlan {
    /// Take the pooled watcher at this index.
    Existing(usize),
    /// Open a new connection.
    Open,
}

/// A watcher kept alive across reloads, with the target it currently serves.
pub struct PooledWatcher {
    pub identity: NacosIdentity,
    /// Zone the watcher's cache is being published under right now. Only used
    /// for reporting (control-plane live signals, logs); the cache itself is
    /// zone-free.
    pub zone: String,
    watcher: Arc<NacosServiceWatcher>,
}

impl PooledWatcher {
    /// Connect, then subscribe to everything the namespace/group currently
    /// lists. New services appear only after a restart or a reload that opens
    /// a fresh watcher — an existing pooled watcher is not re-subscribed,
    /// because a reload that repeats would grow the SDK's listener set per
    /// service without bound.
    pub async fn open(target: &NacosTarget) -> Result<Self> {
        let watcher = Arc::new(
            NacosServiceWatcher::new(
                &target.identity.server_addr,
                &target.identity.namespace,
                &target.identity.group,
            )
            .await?,
        );
        watcher.subscribe_all().await;
        Ok(Self {
            identity: target.identity.clone(),
            zone: target.zone.clone(),
            watcher,
        })
    }

    pub fn cache(&self) -> Arc<DashMap<ServiceKey, Vec<CachedInstance>>> {
        self.watcher.cache()
    }

    /// See `NacosServiceWatcher::last_push`.
    pub fn last_push(&self) -> Arc<AtomicI64> {
        self.watcher.last_push()
    }
}

/// Decide which pooled watcher serves each target.
///
/// Matching consumes: two targets with the same identity take two distinct
/// pooled watchers (that is what a config with two plugins on one server and
/// two zones gets at start-up), and a target with nothing free to take opens
/// a new connection.
pub fn plan_reuse(targets: &[NacosTarget], pool: &[NacosIdentity]) -> Vec<ReusePlan> {
    let mut taken = vec![false; pool.len()];
    targets
        .iter()
        .map(|target| {
            let free = pool
                .iter()
                .enumerate()
                .find(|(index, identity)| !taken[*index] && **identity == target.identity)
                .map(|(index, _)| index);
            match free {
                Some(index) => {
                    taken[index] = true;
                    ReusePlan::Existing(index)
                }
                None => ReusePlan::Open,
            }
        })
        .collect()
}
