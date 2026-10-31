use doggy_dns_basic::ObservabilityConfig;
use opentelemetry::KeyValue;
use opentelemetry_sdk::Resource;
use std::collections::BTreeMap;

/// Build the OTel resource shared by every signal.
///
/// Precedence for `service.name`: config `service_name` → `OTEL_SERVICE_NAME` →
/// `service.name` in `OTEL_RESOURCE_ATTRIBUTES` → [`APP_NAME`].
pub(crate) fn build_resource(
    config: &ObservabilityConfig,
    exposure: &BTreeMap<String, String>,
) -> Resource {
    build_with_env(config, exposure, |name| std::env::var(name).ok())
}

fn build_with_env(
    config: &ObservabilityConfig,
    exposure: &BTreeMap<String, String>,
    env: impl Fn(&str) -> Option<String>,
) -> Resource {
    // Derived node attributes are the defaults; environment and configured
    // attributes override them.
    let mut attributes = exposure.clone();

    if let Some(raw) = env("OTEL_RESOURCE_ATTRIBUTES").filter(|value| !value.trim().is_empty()) {
        for pair in raw.split(',') {
            if let Some((key, value)) = pair.split_once('=') {
                let key = crate::auth::percent_decode(key.trim())
                    .unwrap_or_else(|_| key.trim().to_string());
                let value = crate::auth::percent_decode(value.trim())
                    .unwrap_or_else(|_| value.trim().to_string());
                if !key.is_empty() {
                    attributes.insert(key, value);
                }
            }
        }
    }

    for (key, value) in &config.resource_attributes {
        if !key.trim().is_empty() {
            attributes.insert(key.trim().to_string(), value.trim().to_string());
        }
    }

    let configured_service_name = config
        .service_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    let service_name = configured_service_name
        .or_else(|| {
            env("OTEL_SERVICE_NAME")
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
        .or_else(|| {
            attributes
                .get("service.name")
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_else(|| doggy_dns_plugin::APP_NAME.to_string());

    // An injected `host.name` (config or OTEL_RESOURCE_ATTRIBUTES) also feeds
    // `service.instance.id`, so both stay consistent when the environment has
    // no HOSTNAME.
    let hostname = attributes
        .get("host.name")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| crate::exposure::detect_hostname_with(&env));

    attributes.insert("service.name".to_string(), service_name);
    attributes
        .entry("service.version".to_string())
        .or_insert_with(|| env!("CARGO_PKG_VERSION").to_string());
    attributes
        .entry("service.instance.id".to_string())
        .or_insert_with(|| format!("{hostname}-{}", std::process::id()));
    attributes
        .entry("host.name".to_string())
        .or_insert_with(|| hostname.clone());
    attributes
        .entry("process.pid".to_string())
        .or_insert_with(|| std::process::id().to_string());

    if let Some(environment) = config
        .environment
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        attributes.insert(
            "deployment.environment.name".to_string(),
            environment.to_string(),
        );
    }

    let attributes = attributes
        .into_iter()
        .map(|(key, value)| KeyValue::new(key, value))
        .collect::<Vec<_>>();

    Resource::builder().with_attributes(attributes).build()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]

    use super::*;

    fn env_map(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    fn attr(resource: &Resource, key: &str) -> Option<String> {
        resource
            .iter()
            .find(|(candidate, _)| candidate.as_str() == key)
            .map(|(_, value)| value.to_string())
    }

    #[test]
    fn defaults_to_doggy_dns() {
        let config = ObservabilityConfig::default();
        let resource = build_with_env(
            &config,
            &BTreeMap::new(),
            env_map(&[("HOSTNAME", "test-host")]),
        );

        assert_eq!(
            attr(&resource, "service.name").as_deref(),
            Some(doggy_dns_plugin::APP_NAME)
        );
        assert_eq!(attr(&resource, "host.name").as_deref(), Some("test-host"));
        assert!(attr(&resource, "service.version").is_some());
        assert!(attr(&resource, "service.instance.id").is_some());
    }

    #[test]
    fn otel_service_name_is_used() {
        let config = ObservabilityConfig::default();
        let resource = build_with_env(
            &config,
            &BTreeMap::new(),
            env_map(&[("OTEL_SERVICE_NAME", "dns-edge")]),
        );
        assert_eq!(attr(&resource, "service.name").as_deref(), Some("dns-edge"));
    }

    #[test]
    fn configured_service_name_wins() {
        let config = ObservabilityConfig {
            service_name: Some("dns-prod".to_string()),
            ..ObservabilityConfig::default()
        };
        let resource = build_with_env(
            &config,
            &BTreeMap::new(),
            env_map(&[("OTEL_SERVICE_NAME", "dns-edge")]),
        );
        assert_eq!(attr(&resource, "service.name").as_deref(), Some("dns-prod"));
    }

    #[test]
    fn resource_attributes_merge_with_config_winning() {
        let mut config = ObservabilityConfig {
            enabled: true,
            ..ObservabilityConfig::default()
        };
        config
            .resource_attributes
            .insert("deployment.tier".to_string(), "config".to_string());

        let resource = build_with_env(
            &config,
            &BTreeMap::new(),
            env_map(&[
                ("OTEL_RESOURCE_ATTRIBUTES", "deployment.tier=env,team=dns"),
                ("HOSTNAME", "host-a"),
            ]),
        );

        assert_eq!(attr(&resource, "team").as_deref(), Some("dns"));
        assert_eq!(
            attr(&resource, "deployment.tier").as_deref(),
            Some("config")
        );
    }

    #[test]
    fn configured_host_name_drives_instance_id() {
        let mut config = ObservabilityConfig::default();
        config
            .resource_attributes
            .insert("host.name".to_string(), "node-a".to_string());
        config
            .resource_attributes
            .insert("doggy_dns.ip_addr".to_string(), "10.0.0.5".to_string());
        config
            .resource_attributes
            .insert("doggy_dns.control.port".to_string(), "19090".to_string());
        let resource = build_with_env(&config, &BTreeMap::new(), env_map(&[]));

        assert_eq!(attr(&resource, "host.name").as_deref(), Some("node-a"));
        assert_eq!(
            attr(&resource, "service.instance.id").as_deref(),
            Some(format!("node-a-{}", std::process::id()).as_str())
        );
        assert_eq!(
            attr(&resource, "doggy_dns.ip_addr").as_deref(),
            Some("10.0.0.5")
        );
        assert_eq!(
            attr(&resource, "doggy_dns.control.port").as_deref(),
            Some("19090")
        );
    }

    #[test]
    fn exposure_attributes_are_defaults_overridden_by_config() {
        let mut exposure = BTreeMap::new();
        exposure.insert("doggy_dns.ip_addr".to_string(), "10.0.0.5".to_string());
        exposure.insert("doggy_dns.dns.port".to_string(), "5353".to_string());
        let mut config = ObservabilityConfig::default();
        config
            .resource_attributes
            .insert("doggy_dns.ip_addr".to_string(), "192.0.2.9".to_string());

        let resource = build_with_env(&config, &exposure, env_map(&[]));

        assert_eq!(
            attr(&resource, "doggy_dns.ip_addr").as_deref(),
            Some("192.0.2.9")
        );
        assert_eq!(
            attr(&resource, "doggy_dns.dns.port").as_deref(),
            Some("5353")
        );
    }

    #[test]
    fn environment_maps_to_semconv_attribute() {
        let config = ObservabilityConfig {
            environment: Some("production".to_string()),
            ..ObservabilityConfig::default()
        };
        let resource = build_with_env(&config, &BTreeMap::new(), env_map(&[]));
        assert_eq!(
            attr(&resource, "deployment.environment.name").as_deref(),
            Some("production")
        );
    }

    #[test]
    fn resource_attributes_are_percent_decoded() {
        let config = ObservabilityConfig::default();
        let resource = build_with_env(
            &config,
            &BTreeMap::new(),
            env_map(&[("OTEL_RESOURCE_ATTRIBUTES", "team=dns%20core,zone=eu%2Dwest")]),
        );

        assert_eq!(attr(&resource, "team").as_deref(), Some("dns core"));
        assert_eq!(attr(&resource, "zone").as_deref(), Some("eu-west"));
    }

    #[test]
    fn malformed_resource_attributes_are_ignored() {
        let config = ObservabilityConfig::default();
        let resource = build_with_env(
            &config,
            &BTreeMap::new(),
            env_map(&[("OTEL_RESOURCE_ATTRIBUTES", "good=1,broken,=empty")]),
        );
        assert_eq!(attr(&resource, "good").as_deref(), Some("1"));
    }
}
