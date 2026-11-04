use anyhow::Context;
use doggy_dns_basic::RemoteConfig;
use nacos_sdk::api::config::{
    ConfigChangeListener, ConfigResponse, ConfigService, ConfigServiceBuilder,
};
use nacos_sdk::api::props::ClientProps;
use std::sync::Arc;
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
        // `build()` health-checks the connection and would fail a Nacos that is not
        // reachable yet. Emergency start lets the client come up anyway; the SDK's
        // reconnect plus its 60 s listen re-send deliver the first push once the
        // server answers, so one watcher serves the life of the process.
        let props = ClientProps::new()
            .server_addr(server_addr)
            .namespace(namespace)
            .app_name(doggy_dns_plugin::APP_NAME)
            .config_load_cache_at_start(true);

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
    ///
    /// With `config_load_cache_at_start` the SDK returns a populated cache entry
    /// without validating it against the server, so this can read back the disk
    /// snapshot rather than what Nacos holds now. The reload path never calls it:
    /// a push is the only input.
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
/// A Nacos that is not reachable yet does not cost the watcher: the client starts
/// in emergency mode, so it is built once and the trigger arrives when the server
/// answers.
pub fn spawn_config_pushes(remote: RemoteConfig, pushes: mpsc::UnboundedSender<String>) {
    if !remote.enabled {
        return;
    }
    tokio::spawn(async move {
        let _watcher = match connect(&remote, &pushes).await {
            Ok(watcher) => watcher,
            Err(error) => {
                // Not a retry: a failed build here is a broken configuration, and
                // reconstructing the SDK client would leak its worker tasks, which
                // have no shutdown. The resolver keeps serving DNS without a trigger.
                tracing::warn!(
                    %error,
                    data_id = %remote.data_id,
                    "remote config watcher unavailable; this process has no reload trigger"
                );
                return;
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
