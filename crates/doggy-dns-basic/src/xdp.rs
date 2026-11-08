use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct XdpConfig {
    pub enabled: bool,
    pub interface: String,
    pub cache_size: u32,
    pub negative_ttl: u64,
    pub attach_mode: String,
    pub pin_path: String,
}

impl Default for XdpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interface: String::new(),
            cache_size: 65536,
            negative_ttl: 30,
            attach_mode: "auto".to_string(),
            pin_path: "/sys/fs/bpf/doggy-dns/dns_cache".to_string(),
        }
    }
}
