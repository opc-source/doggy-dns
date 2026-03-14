use nacos_sdk::api::config::{
    ConfigChangeListener, ConfigResponse, ConfigService, ConfigServiceBuilder,
};
use nacos_sdk::api::props::ClientProps;
use std::sync::Arc;

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
            .app_name("dns-filter");

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
            .add_listener(
                self.data_id.clone(),
                self.group.clone(),
                Arc::new(listener),
            )
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
