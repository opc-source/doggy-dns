mod convert;

use crate::auth;
use crate::endpoint::{self, Signal};
use anyhow::{Context, Result};
use doggy_dns_basic::ObservabilityConfig;
use opentelemetry_sdk::Resource;
use prost::Message as _;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

pub(crate) use convert::{
    CollectedProfile, ProfileFrame, ProfileSample, ProfileSymbol, build_export_request,
    resource_from_sdk,
};

/// Wakes the blocking pprof collection thread during shutdown instead of
/// waiting for the full collection window to elapse.
#[derive(Default)]
pub(crate) struct ProfileStopper {
    stopped: Mutex<bool>,
    condvar: Condvar,
}

impl ProfileStopper {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(crate) fn stop(&self) {
        let mut stopped = self
            .stopped
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *stopped = true;
        self.condvar.notify_all();
    }

    /// Wait up to `duration`; returns true when a stop was requested.
    fn wait(&self, duration: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + duration;
        let mut stopped = self
            .stopped
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        while !*stopped {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (guard, _) = self
                .condvar
                .wait_timeout(stopped, remaining)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            stopped = guard;
        }

        true
    }
}

/// HTTP exporter for OTLP CPU profiles (`/v1/profiles`).
pub(crate) struct ProfileExporter {
    url: String,
    headers: HeaderMap,
    client: reqwest::Client,
    resource: opentelemetry_proto::tonic::resource::v1::Resource,
    interval: Duration,
    duration: Duration,
    frequency: i32,
}

impl ProfileExporter {
    pub(crate) fn new(config: &ObservabilityConfig, resource: &Resource) -> Result<Self> {
        let url = endpoint::resolve_signal_endpoint(config, Signal::Profiles)?;
        let resolved_headers = auth::resolve_headers(config)?;

        let mut headers = HeaderMap::new();
        for (name, value) in resolved_headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .with_context(|| format!("invalid OTLP header name: {name}"))?;
            let value = HeaderValue::from_str(&value)
                .with_context(|| format!("invalid OTLP header value for {name}"))?;
            headers.insert(name, value);
        }
        // Profiles must always be protobuf; override a user-supplied content type.
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/x-protobuf"),
        );

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .build()
            .context("failed to build profile HTTP client")?;

        Ok(Self {
            url,
            headers,
            client,
            resource: resource_from_sdk(resource),
            interval: Duration::from_secs(config.profiles.interval_secs),
            duration: Duration::from_secs(config.profiles.duration_secs),
            frequency: config.profiles.frequency_hz,
        })
    }

    /// Spawn the periodic CPU profiling task. Must be called from a Tokio runtime.
    pub(crate) fn spawn(self, stopper: Arc<ProfileStopper>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(self.interval);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
            // The first tick completes immediately; wait one full interval before
            // collecting the first profile.
            ticker.tick().await;

            loop {
                ticker.tick().await;

                let frequency = self.frequency;
                let duration = self.duration;
                let stop = Arc::clone(&stopper);
                let collected = match tokio::task::spawn_blocking(move || {
                    collect_profile(frequency, duration, &stop)
                })
                .await
                {
                    Ok(Ok(Some(profile))) => profile,
                    Ok(Ok(None)) => break,
                    Ok(Err(error)) => {
                        tracing::warn!(error = %error, "failed to collect CPU profile");
                        continue;
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "CPU profile collection task failed");
                        continue;
                    }
                };

                if collected.samples.is_empty() {
                    tracing::debug!("CPU profile had no samples; skipping export");
                    continue;
                }

                let request = build_export_request(self.resource.clone(), collected);
                let body = request.encode_to_vec();
                let body_len = body.len();

                match self
                    .client
                    .post(&self.url)
                    .headers(self.headers.clone())
                    .body(body)
                    .send()
                    .await
                {
                    Ok(response) if response.status().is_success() => {
                        tracing::debug!(
                            bytes = body_len,
                            url = %self.url,
                            "exported CPU profile"
                        );
                    }
                    Ok(response) => {
                        let status = response.status();
                        let body = response.text().await.unwrap_or_default();
                        let body = body.chars().take(512).collect::<String>();
                        tracing::warn!(
                            %status,
                            body = %body,
                            url = %self.url,
                            "profile export rejected"
                        );
                    }
                    Err(error) => {
                        tracing::warn!(
                            error = %error,
                            url = %self.url,
                            "failed to export profile"
                        );
                    }
                }
            }
        })
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn collect_profile(
    frequency: i32,
    duration: Duration,
    stopper: &ProfileStopper,
) -> Result<Option<CollectedProfile>> {
    use std::time::UNIX_EPOCH;

    let guard = pprof::ProfilerGuardBuilder::default()
        .frequency(frequency)
        .blocklist(&["libc", "libgcc", "pthread", "vdso"])
        .build()
        .context("failed to start pprof profiler")?;

    // Block this dedicated profiling thread for the collection window, but wake
    // immediately when shutdown asks us to stop.
    if stopper.wait(duration) {
        return Ok(None);
    }

    let report = guard
        .report()
        .build()
        .context("failed to build pprof report")?;
    let timing = &report.timing;

    let samples = report
        .data
        .iter()
        .map(|(frames, count)| ProfileSample {
            frames: frames
                .frames
                .iter()
                .map(|frame| ProfileFrame {
                    symbols: frame
                        .iter()
                        .map(|symbol| ProfileSymbol {
                            name: symbol.name(),
                            system_name: symbol.sys_name().into_owned(),
                            filename: symbol.filename().into_owned(),
                            line: i64::from(symbol.lineno()),
                            address: symbol.addr.map_or(0, |addr| addr as usize as u64),
                        })
                        .collect(),
                })
                .collect(),
            count: *count as i64,
            thread_name: Some(frames.thread_name_or_id()),
        })
        .collect();

    Ok(Some(CollectedProfile {
        samples,
        time_unix_nano: timing
            .start_time
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64,
        duration_nano: timing.duration.as_nanos() as u64,
        frequency: timing.frequency,
    }))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn collect_profile(
    _frequency: i32,
    _duration: Duration,
    _stopper: &ProfileStopper,
) -> Result<Option<CollectedProfile>> {
    anyhow::bail!("CPU profiles are only supported on Linux and macOS")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]

    use super::*;
    use doggy_dns_basic::{BasicAuthConfig, ObservabilityConfig};

    fn config(endpoint: &str) -> ObservabilityConfig {
        let mut config = ObservabilityConfig {
            enabled: true,
            endpoint: endpoint.to_string(),
            basic_auth: Some(BasicAuthConfig {
                username: "root@example.com".to_string(),
                password: "secret".to_string(),
            }),
            ..ObservabilityConfig::default()
        };
        config.profiles.enabled = true;
        config
    }

    fn resource() -> Resource {
        Resource::builder()
            .with_service_name(doggy_dns_plugin::APP_NAME)
            .build()
    }

    #[test]
    fn stopper_returns_true_when_stopped() {
        let stopper = ProfileStopper::new();
        let waiter = Arc::clone(&stopper);
        let handle = std::thread::spawn(move || waiter.wait(Duration::from_secs(30)));

        std::thread::sleep(Duration::from_millis(20));
        stopper.stop();
        assert!(handle.join().unwrap());
    }

    #[test]
    fn stopper_times_out_when_not_stopped() {
        let stopper = ProfileStopper::new();
        assert!(!stopper.wait(Duration::from_millis(1)));
    }

    #[test]
    fn exporter_uses_profiles_endpoint() {
        let exporter =
            ProfileExporter::new(&config("http://127.0.0.1:5080/api/default"), &resource())
                .unwrap();

        assert_eq!(
            exporter.url,
            "http://127.0.0.1:5080/api/default/v1/profiles"
        );
        assert_eq!(exporter.interval, Duration::from_secs(60));
        assert_eq!(exporter.duration, Duration::from_secs(10));
        assert_eq!(exporter.frequency, 99);
    }

    #[test]
    fn exporter_sets_content_type_and_auth_headers() {
        let exporter =
            ProfileExporter::new(&config("http://127.0.0.1:5080/api/default"), &resource())
                .unwrap();

        assert_eq!(
            exporter.headers.get(CONTENT_TYPE).unwrap(),
            "application/x-protobuf"
        );
        let authorization = exporter.headers.get("authorization").unwrap();
        assert!(authorization.to_str().unwrap().starts_with("Basic "));
    }

    #[test]
    fn exporter_forces_protobuf_content_type() {
        let mut config = config("http://127.0.0.1:5080/api/default");
        config
            .headers
            .insert("content-type".to_string(), "application/json".to_string());

        let exporter = ProfileExporter::new(&config, &resource()).unwrap();
        assert_eq!(
            exporter.headers.get(CONTENT_TYPE).unwrap(),
            "application/x-protobuf"
        );
    }

    #[test]
    fn exporter_resource_contains_service_name() {
        let exporter =
            ProfileExporter::new(&config("http://127.0.0.1:5080/api/default"), &resource())
                .unwrap();

        let resource = &exporter.resource;
        assert!(resource.attributes.iter().any(|attribute| {
            attribute.key == "service.name"
                && matches!(
                    attribute.value.as_ref().and_then(|value| value.value.as_ref()),
                    Some(opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(
                        value
                    )) if value == doggy_dns_plugin::APP_NAME
                )
        }));
    }

    #[test]
    fn invalid_signal_endpoint_is_rejected() {
        let exporter = ProfileExporter::new(&config("not-a-url"), &resource());
        assert!(exporter.is_err());
    }
}
