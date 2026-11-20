//! Authority chain wiring: builds the plugins `[[plugins]]` describes.

use anyhow::{Context, Result};
use doggy_dns_core::config::net::parse_upstream;
use doggy_dns_core::config::{
    DoggyDnsConfig, NACOS_DEFAULT_DNS_ZONE, NACOS_DEFAULT_GROUP, NACOS_DEFAULT_NAMESPACE,
    NACOS_DEFAULT_SERVER_ADDR, PluginKind,
};
use doggy_dns_nacos::authority::NacosAuthority;
use doggy_dns_nacos::watcher::{NacosServiceWatcher, ServiceFilter};
use doggy_dns_plugin::authority_chain::AuthorityChain;
use doggy_dns_plugin::builtin::forward::ForwardAuthority;
use doggy_dns_plugin::builtin::native::NativeAuthority;
use hickory_server::zone_handler::ZoneHandler;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// The upstreams a forward plugin falls back to when `[[plugins]].upstream` is
/// unset and when no `[[plugins]]` section exists at all.
const DEFAULT_FORWARD_UPSTREAM: [&str; 2] = ["8.8.8.8:53", "1.1.1.1:53"];

/// [`DEFAULT_FORWARD_UPSTREAM`] in the shape `ForwardAuthority::new` takes, so
/// neither fallback site repeats the conversion.
fn default_forward_upstreams() -> Result<Vec<SocketAddr>> {
    DEFAULT_FORWARD_UPSTREAM
        .iter()
        .map(|upstream| parse_upstream(upstream).context("invalid built-in forward upstream"))
        .collect()
}

/// Cache size and TTLs of the default forward plugin `build_authority_chain`
/// builds when no `[[plugins]]` section is configured. Nothing reads the
/// `[[plugins]]` field defaults (`cache_size` 4096, `min_ttl` 10, `max_ttl` 300)
/// for this built-in, so they live here instead.
const DEFAULT_FORWARD_CACHE_SIZE: u64 = 1024;
const DEFAULT_FORWARD_MIN_TTL: Duration = Duration::from_secs(30);
const DEFAULT_FORWARD_MAX_TTL: Duration = Duration::from_secs(600);

pub(crate) async fn build_authority_chain(
    config: &DoggyDnsConfig,
) -> Result<(Vec<(String, Arc<NacosServiceWatcher>)>, AuthorityChain)> {
    let mut handlers: Vec<(String, Arc<dyn ZoneHandler>)> = Vec::new();
    let mut watchers: Vec<(String, Arc<NacosServiceWatcher>)> = Vec::new();

    if config.plugins.is_empty() {
        tracing::info!("no plugins configured, defaulting to forward");
        let forward = ForwardAuthority::new(
            default_forward_upstreams()?,
            DEFAULT_FORWARD_CACHE_SIZE,
            DEFAULT_FORWARD_MIN_TTL,
            DEFAULT_FORWARD_MAX_TTL,
        )
        .await?;
        handlers.push(("forward[default]".to_string(), Arc::new(forward)));
        return Ok((watchers, AuthorityChain::new(handlers)));
    }

    for (i, plugin) in config.plugins.iter().enumerate() {
        if !plugin.enabled {
            tracing::info!(index = i, kind = ?plugin.kind, "skipping disabled plugin");
            continue;
        }

        match plugin.kind {
            PluginKind::Nacos => {
                let addr = plugin
                    .server_addr
                    .as_deref()
                    .unwrap_or(NACOS_DEFAULT_SERVER_ADDR);
                let ns = plugin
                    .namespace
                    .as_deref()
                    .unwrap_or(NACOS_DEFAULT_NAMESPACE);
                let grp = plugin.group.as_deref().unwrap_or(NACOS_DEFAULT_GROUP);
                let zone = plugin.dns_zone.as_deref().unwrap_or(NACOS_DEFAULT_DNS_ZONE);
                let filter = ServiceFilter::from_config(plugin.service_pattern.as_deref())
                    .context(format!("plugins[{i}].service_pattern"))?;

                let watcher = Arc::new(NacosServiceWatcher::new(addr, ns, grp, filter).await?);

                let name = format!("nacos[{}]", zone);
                let authority = NacosAuthority::new(watcher.clone(), zone, grp, ns, plugin.ttl);
                handlers.push((name.clone(), Arc::new(authority)));
                watchers.push((zone.to_string(), watcher));
                tracing::info!(plugin = %name, zone = zone, addr = addr, "loaded plugin");
            }
            PluginKind::Native => {
                let authority = NativeAuthority::new(
                    plugin.cache_size as u64,
                    Duration::from_secs(plugin.min_ttl as u64),
                    Duration::from_secs(plugin.max_ttl as u64),
                )
                .await?;
                let name = "native".to_string();
                handlers.push((name.clone(), Arc::new(authority)));
                tracing::info!(plugin = %name, "loaded plugin");
            }
            PluginKind::Forward => {
                let (addrs, configured) = match &plugin.upstream {
                    Some(values) => (
                        values
                            .iter()
                            .map(|value| {
                                parse_upstream(value)
                                    .with_context(|| format!("plugins[{i}].upstream"))
                            })
                            .collect::<Result<Vec<_>>>()?,
                        values.join(","),
                    ),
                    None => (
                        default_forward_upstreams()?,
                        DEFAULT_FORWARD_UPSTREAM.join(","),
                    ),
                };
                let name = format!("forward[{configured}]");
                let authority = ForwardAuthority::new(
                    addrs,
                    plugin.cache_size as u64,
                    Duration::from_secs(plugin.min_ttl as u64),
                    Duration::from_secs(plugin.max_ttl as u64),
                )
                .await?;
                handlers.push((name.clone(), Arc::new(authority)));
                tracing::info!(plugin = %name, "loaded plugin");
            }
        }
    }

    tracing::info!(chain_length = handlers.len(), "authority chain built");
    Ok((watchers, AuthorityChain::new(handlers)))
}
