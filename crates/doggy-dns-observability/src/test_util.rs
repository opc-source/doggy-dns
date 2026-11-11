//! Test-only helpers shared by this crate's inline test modules.

use std::collections::HashMap;

pub(crate) fn env_map(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect();
    move |name| map.get(name).cloned()
}
