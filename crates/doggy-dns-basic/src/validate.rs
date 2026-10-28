//! Validation rules for `DoggyDnsConfig` and its subsections.

use crate::DoggyDnsConfig;
use crate::observability::ObservabilityConfig;
use crate::plugin::{PluginConfig, PluginKind};
use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};

pub fn validate_config(config: &DoggyDnsConfig) -> anyhow::Result<()> {
    anyhow::ensure!(
        config.server.worker_threads > 0,
        "server.worker_threads must be greater than 0"
    );
    if let Some(tls) = &config.server.tls
        && tls.enabled
    {
        anyhow::ensure!(
            std::path::Path::new(&tls.cert_path).exists(),
            "TLS cert_path does not exist: {}",
            tls.cert_path
        );
        anyhow::ensure!(
            std::path::Path::new(&tls.key_path).exists(),
            "TLS key_path does not exist: {}",
            tls.key_path
        );
    }
    if let Some(https) = &config.server.https
        && https.enabled
    {
        anyhow::ensure!(
            std::path::Path::new(&https.cert_path).exists(),
            "HTTPS cert_path does not exist: {}",
            https.cert_path
        );
        anyhow::ensure!(
            std::path::Path::new(&https.key_path).exists(),
            "HTTPS key_path does not exist: {}",
            https.key_path
        );
    }
    if config.remote_config.enabled {
        anyhow::ensure!(
            !config.remote_config.server_addr.is_empty(),
            "remote_config.server_addr is required when remote_config is enabled"
        );
        anyhow::ensure!(
            !config.remote_config.data_id.is_empty(),
            "remote_config.data_id is required when remote_config is enabled"
        );
    }
    validate_plugins(&config.plugins)?;
    validate_observability(&config.observability)?;
    if config.xdp.enabled {
        anyhow::ensure!(
            config.xdp.cache_size > 0,
            "xdp.cache_size must be greater than 0"
        );
        anyhow::ensure!(
            config.xdp.negative_ttl > 0,
            "xdp.negative_ttl must be greater than 0"
        );
        anyhow::ensure!(
            matches!(config.xdp.attach_mode.as_str(), "auto" | "manual"),
            "xdp.attach_mode must be 'auto' or 'manual'"
        );
        anyhow::ensure!(
            config.xdp.pin_path.starts_with("/sys/fs/bpf/"),
            "xdp.pin_path must be under /sys/fs/bpf/"
        );
    }
    if config.mcp.enabled {
        let addr = format!("{}:{}", config.mcp.listen_addr, config.mcp.port);
        addr.parse::<SocketAddr>()
            .map_err(|_| anyhow::anyhow!("mcp.listen_addr is not a valid IP address: {}", addr))?;
    }
    Ok(())
}

fn validate_plugins(plugins: &[PluginConfig]) -> anyhow::Result<()> {
    if plugins.is_empty() {
        return Ok(());
    }
    anyhow::ensure!(
        plugins.iter().any(|plugin| plugin.enabled),
        "at least one plugin must be enabled when [[plugins]] is configured"
    );

    let mut zones: HashSet<String> = HashSet::new();
    for (index, plugin) in plugins.iter().enumerate() {
        if !plugin.enabled {
            continue;
        }
        let label = format!("plugins[{index}] ({})", kind_name(&plugin.kind));
        match plugin.kind {
            PluginKind::Nacos => {
                for (field, value) in [
                    ("server_addr", &plugin.server_addr),
                    ("namespace", &plugin.namespace),
                    ("group", &plugin.group),
                    ("dns_zone", &plugin.dns_zone),
                ] {
                    anyhow::ensure!(
                        value
                            .as_deref()
                            .is_some_and(|value| !value.trim().is_empty()),
                        "{label}.{field} is required for an enabled nacos plugin"
                    );
                }
                let configured = plugin.dns_zone.as_deref().unwrap_or_default();
                let zone = configured.strip_suffix('.').unwrap_or(configured);
                validate_dns_zone(&label, zone)?;
                let zone = zone.to_ascii_lowercase();
                anyhow::ensure!(
                    zones.insert(zone.clone()),
                    "duplicate nacos dns_zone '{zone}' at {label}; each enabled nacos plugin needs its own zone"
                );
            }
            PluginKind::Native => validate_resolver_ttls(&label, plugin)?,
            PluginKind::Forward => {
                validate_resolver_ttls(&label, plugin)?;
                validate_upstream(&label, plugin)?;
            }
        }
    }
    Ok(())
}

fn kind_name(kind: &PluginKind) -> &'static str {
    match kind {
        PluginKind::Nacos => "nacos",
        PluginKind::Native => "native",
        PluginKind::Forward => "forward",
    }
}

fn validate_dns_zone(label: &str, zone: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!zone.is_empty(), "{label}.dns_zone must not be empty");
    anyhow::ensure!(
        zone.len() <= 253,
        "{label}.dns_zone must be at most 253 bytes"
    );
    for part in zone.split('.') {
        anyhow::ensure!(
            !part.is_empty(),
            "{label}.dns_zone must not contain empty labels"
        );
        anyhow::ensure!(
            part.len() <= 63,
            "{label}.dns_zone label '{part}' must be at most 63 bytes"
        );
        anyhow::ensure!(
            part.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
            "{label}.dns_zone label '{part}' may only contain letters, digits, '-' and '_'"
        );
    }
    Ok(())
}

fn validate_resolver_ttls(label: &str, plugin: &PluginConfig) -> anyhow::Result<()> {
    anyhow::ensure!(
        plugin.min_ttl <= plugin.max_ttl,
        "{label}.min_ttl ({}) must not exceed max_ttl ({})",
        plugin.min_ttl,
        plugin.max_ttl
    );
    Ok(())
}

fn validate_upstream(label: &str, plugin: &PluginConfig) -> anyhow::Result<()> {
    let Some(upstream) = &plugin.upstream else {
        return Ok(());
    };
    anyhow::ensure!(
        !upstream.is_empty(),
        "{label}.upstream must not be empty; omit it to use the default servers"
    );
    for address in upstream {
        anyhow::ensure!(
            address.parse::<SocketAddr>().is_ok() || address.parse::<IpAddr>().is_ok(),
            "{label}.upstream entry '{address}' is not an IP address or IP:port"
        );
    }
    Ok(())
}

fn validate_observability(config: &ObservabilityConfig) -> anyhow::Result<()> {
    if !config.enabled {
        return Ok(());
    }

    if let Some(name) = &config.service_name {
        anyhow::ensure!(
            !name.trim().is_empty(),
            "observability.service_name must not be empty"
        );
    }

    let any_signal = config.logs.enabled
        || config.metrics.enabled
        || config.traces.enabled
        || config.profiles.enabled;
    anyhow::ensure!(
        any_signal,
        "observability.enabled is true but no signal (logs/metrics/traces/profiles) is enabled"
    );

    if !config.endpoint.trim().is_empty() {
        validate_observability_url("observability.endpoint", &config.endpoint, false)?;
    }
    for (name, endpoint) in [
        (
            "observability.logs.endpoint",
            config.logs.endpoint.as_deref(),
        ),
        (
            "observability.metrics.endpoint",
            config.metrics.endpoint.as_deref(),
        ),
        (
            "observability.traces.endpoint",
            config.traces.endpoint.as_deref(),
        ),
        (
            "observability.profiles.endpoint",
            config.profiles.endpoint.as_deref(),
        ),
    ] {
        if let Some(endpoint) = endpoint
            && !endpoint.trim().is_empty()
        {
            validate_observability_url(name, endpoint, true)?;
        }
    }

    anyhow::ensure!(
        config.timeout_secs > 0,
        "observability.timeout_secs must be greater than 0"
    );

    let level = config.logs.level.to_ascii_lowercase();
    anyhow::ensure!(
        matches!(
            level.as_str(),
            "trace" | "debug" | "info" | "warn" | "error"
        ),
        "observability.logs.level must be one of trace, debug, info, warn, error"
    );

    anyhow::ensure!(
        config.metrics.interval_secs > 0,
        "observability.metrics.interval_secs must be greater than 0"
    );

    anyhow::ensure!(
        config.traces.sample_ratio.is_finite() && (0.0..=1.0).contains(&config.traces.sample_ratio),
        "observability.traces.sample_ratio must be between 0.0 and 1.0"
    );
    anyhow::ensure!(
        config.traces.max_queue_size > 0,
        "observability.traces.max_queue_size must be greater than 0"
    );
    anyhow::ensure!(
        config.traces.max_export_batch_size > 0
            && config.traces.max_export_batch_size <= config.traces.max_queue_size,
        "observability.traces.max_export_batch_size must be in 1..=max_queue_size"
    );
    anyhow::ensure!(
        config.traces.scheduled_delay_ms > 0,
        "observability.traces.scheduled_delay_ms must be greater than 0"
    );

    anyhow::ensure!(
        config.profiles.interval_secs > 0,
        "observability.profiles.interval_secs must be greater than 0"
    );
    anyhow::ensure!(
        config.profiles.duration_secs > 0
            && config.profiles.duration_secs <= config.profiles.interval_secs,
        "observability.profiles.duration_secs must be in 1..=interval_secs"
    );
    anyhow::ensure!(
        (1..=1000).contains(&config.profiles.frequency_hz),
        "observability.profiles.frequency_hz must be between 1 and 1000"
    );
    if config.profiles.enabled {
        anyhow::ensure!(
            cfg!(any(target_os = "linux", target_os = "macos")),
            "observability.profiles is only supported on Linux and macOS"
        );
    }

    if let Some(auth) = &config.basic_auth {
        anyhow::ensure!(
            !auth.username.trim().is_empty() && !auth.password.is_empty(),
            "observability.basic_auth username and password must not be empty"
        );
    }

    Ok(())
}

fn validate_observability_url(
    name: &str,
    value: &str,
    allow_signal_path: bool,
) -> anyhow::Result<()> {
    let value = value.trim();
    anyhow::ensure!(
        value.starts_with("http://") || value.starts_with("https://"),
        "{name} must start with http:// or https://"
    );
    anyhow::ensure!(
        !value.contains(char::is_whitespace),
        "{name} must not contain whitespace"
    );
    if !allow_signal_path {
        anyhow::ensure!(
            !value.contains(['?', '#']),
            "{name} must not contain a query string or fragment"
        );
        anyhow::ensure!(
            !value.trim_end_matches('/').ends_with("/v1"),
            "{name} must be the OTLP base URL without /v1"
        );
        for signal in ["logs", "metrics", "traces", "profiles"] {
            anyhow::ensure!(
                !value.ends_with(&format!("/v1/{signal}")),
                "{name} must be the OTLP base URL without /v1/{signal}; use the signal-specific endpoint for a full URL"
            );
        }
    }
    Ok(())
}
