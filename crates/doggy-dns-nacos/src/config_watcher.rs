use anyhow::Context;
use doggy_dns_basic::RemoteConfig;
use nacos_sdk::api::config::{
    ConfigChangeListener, ConfigResponse, ConfigService, ConfigServiceBuilder,
};
use nacos_sdk::api::props::ClientProps;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Watches Nacos config service for changes and triggers a callback.
pub struct NacosConfigWatcher {
    config_service: ConfigService,
    data_id: String,
    group: String,
}

struct ConfigReloadListener<F: Fn(String) + Send + Sync + 'static> {
    callback: F,
}

impl<F: Fn(String) + Send + Sync + 'static> ConfigChangeListener for ConfigReloadListener<F> {
    fn notify(&self, config_resp: ConfigResponse) {
        let content = config_resp.content().to_string();
        tracing::info!(
            data_id = %config_resp.data_id(),
            "received config update from nacos"
        );
        (self.callback)(content);
    }
}

impl NacosConfigWatcher {
    pub async fn new(
        server_addr: &str,
        namespace: &str,
        group: &str,
        data_id: &str,
    ) -> anyhow::Result<Self> {
        let props = ClientProps::new()
            .server_addr(server_addr)
            .namespace(namespace)
            .app_name(doggy_dns_plugin::APP_NAME);

        let config_service = ConfigServiceBuilder::new(props).build().await?;

        Ok(Self {
            config_service,
            data_id: data_id.to_string(),
            group: group.to_string(),
        })
    }

    /// Start listening for config changes. The callback receives the new TOML content.
    pub async fn start_watching<F>(&self, callback: F) -> anyhow::Result<()>
    where
        F: Fn(String) + Send + Sync + 'static,
    {
        let listener = ConfigReloadListener { callback };

        self.config_service
            .add_listener(self.data_id.clone(), self.group.clone(), Arc::new(listener))
            .await?;

        tracing::info!(
            data_id = %self.data_id,
            group = %self.group,
            "watching nacos config for changes"
        );

        Ok(())
    }

    /// Get current config content.
    pub async fn get_current_config(&self) -> anyhow::Result<String> {
        let resp = self
            .config_service
            .get_config(self.data_id.clone(), self.group.clone())
            .await?;
        Ok(resp.content().to_string())
    }
}

/// `[remote_config]`: one Nacos config watcher for the life of the process.
///
/// The SDK hands pushed content to a synchronous callback, which is exactly what
/// an unbounded channel send needs: the callback queues the text and returns, so
/// a multi-line edit costs the reload loop one fold, not several.
///
/// Connection failure is non-fatal and retried every 30 s: the resolver keeps
/// serving DNS without its remote trigger.
pub fn spawn_config_pushes(remote: RemoteConfig, pushes: mpsc::UnboundedSender<String>) {
    if !remote.enabled {
        return;
    }
    tokio::spawn(async move {
        let _watcher = loop {
            match connect(&remote, &pushes).await {
                Ok(watcher) => break watcher,
                Err(error) => {
                    tracing::warn!(
                        %error,
                        data_id = %remote.data_id,
                        "remote config watcher unavailable, retrying in 30s"
                    );
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            }
        };

        tracing::info!(
            data_id = %remote.data_id,
            group = %remote.group,
            "config reload listening on nacos push"
        );
        // Park holding the watcher: the SDK pushes to the listener registered on
        // this watcher's config service, so dropping it would end the trigger.
        // Shutdown cancels this task, and that is when it goes.
        std::future::pending::<()>().await;
    });
}

/// Connect, then subscribe so every pushed content lands on `pushes`.
async fn connect(
    remote: &RemoteConfig,
    pushes: &mpsc::UnboundedSender<String>,
) -> anyhow::Result<NacosConfigWatcher> {
    let watcher = NacosConfigWatcher::new(
        &remote.server_addr,
        &remote.namespace,
        &remote.group,
        &remote.data_id,
    )
    .await
    .context("failed to connect the remote config watcher")?;

    let pushes = pushes.clone();
    watcher
        .start_watching(move |content| {
            let _ = pushes.send(content);
        })
        .await
        .context("failed to subscribe to the remote config data id")?;

    Ok(watcher)
}
