use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct XdpConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub interface: String,
    #[serde(default = "default_xdp_cache_size")]
    pub cache_size: u32,
    #[serde(default = "default_xdp_negative_ttl")]
    pub negative_ttl: u64,
    #[serde(default = "default_xdp_attach_mode")]
    pub attach_mode: String,
    #[serde(default = "default_xdp_pin_path")]
    pub pin_path: String,
}

impl Default for XdpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interface: String::new(),
            cache_size: default_xdp_cache_size(),
            negative_ttl: default_xdp_negative_ttl(),
            attach_mode: default_xdp_attach_mode(),
            pin_path: default_xdp_pin_path(),
        }
    }
}

fn default_xdp_cache_size() -> u32 {
    65536
}
fn default_xdp_negative_ttl() -> u64 {
    30
}
fn default_xdp_attach_mode() -> String {
    "auto".to_string()
}
fn default_xdp_pin_path() -> String {
    "/sys/fs/bpf/doggy-dns/dns_cache".to_string()
}
