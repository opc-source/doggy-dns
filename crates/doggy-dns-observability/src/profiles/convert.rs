use opentelemetry_proto::tonic::collector::profiles::v1development::ExportProfilesServiceRequest;
use opentelemetry_proto::tonic::common::v1::{AnyValue, InstrumentationScope, any_value};
use opentelemetry_proto::tonic::profiles::v1development::{
    Function, KeyValueAndUnit, Line, Link, Location, Mapping, Profile, ProfilesDictionary,
    ResourceProfiles, Sample, ScopeProfiles, Stack, ValueType,
};
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::transform::common::tonic::ResourceAttributesWithSchema;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A CPU profile converted from pprof into the intermediate shape used by the
/// OTLP profiles dictionary builder.
#[derive(Debug, Clone)]
pub(crate) struct CollectedProfile {
    pub samples: Vec<ProfileSample>,
    pub time_unix_nano: u64,
    pub duration_nano: u64,
    pub frequency: i32,
}

#[derive(Debug, Clone)]
pub(crate) struct ProfileSample {
    /// Leaf frame first.
    pub frames: Vec<ProfileFrame>,
    pub count: i64,
    pub thread_name: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ProfileFrame {
    /// Inline frames, innermost first.
    pub symbols: Vec<ProfileSymbol>,
}

#[derive(Debug, Clone)]
pub(crate) struct ProfileSymbol {
    pub name: String,
    pub system_name: String,
    pub filename: String,
    pub line: i64,
    pub address: u64,
}

/// Convert the SDK resource into the proto resource used by the profiles request.
pub(crate) fn resource_from_sdk(resource: &opentelemetry_sdk::Resource) -> Resource {
    let attributes = ResourceAttributesWithSchema::from(resource);
    Resource {
        attributes: attributes.attributes.0,
        dropped_attributes_count: 0,
        entity_refs: Vec::new(),
    }
}

/// Build an `ExportProfilesServiceRequest` for a CPU profile.
///
/// The OTLP profiles dictionary requires a zero value at index 0 of every
/// table, including the string table. OpenObserve's alpha profiles implementation
/// validates this strictly.
pub(crate) fn build_export_request(
    resource: Resource,
    collected: CollectedProfile,
) -> ExportProfilesServiceRequest {
    let frequency = collected.frequency.max(1);
    let period_ns = 1_000_000_000_i64 / i64::from(frequency);
    let mut dictionary = DictionaryBuilder::new();

    let mut samples = Vec::with_capacity(collected.samples.len());
    for sample in &collected.samples {
        if sample.count <= 0 {
            continue;
        }
        let Some(stack_index) = dictionary.stack(sample) else {
            continue;
        };
        let mut attribute_indices = Vec::new();
        if let Some(thread_name) = sample
            .thread_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            attribute_indices.push(dictionary.thread_attribute(thread_name));
        }
        samples.push(Sample {
            stack_index,
            attribute_indices,
            link_index: 0,
            values: vec![sample.count.saturating_mul(period_ns)],
            timestamps_unix_nano: Vec::new(),
        });
    }

    let cpu = dictionary.intern("cpu");
    let nanoseconds = dictionary.intern("nanoseconds");
    let sample_type = ValueType {
        type_strindex: cpu,
        unit_strindex: nanoseconds,
    };

    let profile = Profile {
        sample_type: Some(sample_type),
        samples,
        time_unix_nano: collected.time_unix_nano,
        duration_nano: collected.duration_nano,
        period_type: Some(sample_type),
        period: period_ns,
        profile_id: new_profile_id(),
        dropped_attributes_count: 0,
        original_payload_format: String::new(),
        original_payload: Vec::new(),
        attribute_indices: Vec::new(),
    };

    let resource_profiles = ResourceProfiles {
        resource: Some(resource),
        scope_profiles: vec![ScopeProfiles {
            scope: Some(InstrumentationScope {
                name: "doggy-dns".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                attributes: Vec::new(),
                dropped_attributes_count: 0,
            }),
            profiles: vec![profile],
            schema_url: String::new(),
        }],
        schema_url: String::new(),
    };

    ExportProfilesServiceRequest {
        resource_profiles: vec![resource_profiles],
        dictionary: Some(dictionary.build()),
    }
}

fn new_profile_id() -> Vec<u8> {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or_default();

    let mut id = Vec::with_capacity(16);
    id.extend_from_slice(&timestamp.to_be_bytes());
    id.extend_from_slice(&sequence.to_be_bytes());
    id
}

#[derive(Default)]
struct DictionaryBuilder {
    strings: Vec<String>,
    string_lookup: HashMap<String, i32>,
    functions: Vec<Function>,
    function_lookup: HashMap<(i32, i32, i32), i32>,
    locations: Vec<Location>,
    location_lookup: HashMap<(u64, Vec<(i32, i64)>), i32>,
    stacks: Vec<Stack>,
    stack_lookup: HashMap<Vec<i32>, i32>,
    attributes: Vec<KeyValueAndUnit>,
    attribute_lookup: HashMap<(i32, String), i32>,
}

impl DictionaryBuilder {
    fn new() -> Self {
        let mut builder = Self::default();
        builder.strings.push(String::new());
        builder.string_lookup.insert(String::new(), 0);
        builder.functions.push(Function::default());
        builder.locations.push(Location::default());
        builder.stacks.push(Stack::default());
        builder.attributes.push(KeyValueAndUnit::default());
        builder
    }

    fn intern(&mut self, value: &str) -> i32 {
        if let Some(index) = self.string_lookup.get(value) {
            return *index;
        }
        let index = self.strings.len() as i32;
        self.strings.push(value.to_string());
        self.string_lookup.insert(value.to_string(), index);
        index
    }

    fn function(&mut self, symbol: &ProfileSymbol) -> i32 {
        let name = self.intern(&symbol.name);
        let system_name = self.intern(&symbol.system_name);
        let filename = self.intern(&symbol.filename);
        let key = (name, system_name, filename);
        if let Some(index) = self.function_lookup.get(&key) {
            return *index;
        }

        let index = self.functions.len() as i32;
        self.functions.push(Function {
            name_strindex: name,
            system_name_strindex: system_name,
            filename_strindex: filename,
            start_line: 0,
        });
        self.function_lookup.insert(key, index);
        index
    }

    fn location(&mut self, frame: &ProfileFrame) -> Option<i32> {
        if frame.symbols.is_empty() {
            return None;
        }

        let mut lines = Vec::with_capacity(frame.symbols.len());
        let mut line_key = Vec::with_capacity(frame.symbols.len());
        for symbol in &frame.symbols {
            let function_index = self.function(symbol);
            lines.push(Line {
                function_index,
                line: symbol.line,
                column: 0,
            });
            line_key.push((function_index, symbol.line));
        }

        let address = frame
            .symbols
            .first()
            .map(|symbol| symbol.address)
            .unwrap_or(0);
        let key = (address, line_key);
        if let Some(index) = self.location_lookup.get(&key) {
            return Some(*index);
        }

        let index = self.locations.len() as i32;
        self.locations.push(Location {
            mapping_index: 0,
            address,
            lines,
            attribute_indices: Vec::new(),
        });
        self.location_lookup.insert(key, index);
        Some(index)
    }

    fn stack(&mut self, sample: &ProfileSample) -> Option<i32> {
        let location_indices = sample
            .frames
            .iter()
            .filter_map(|frame| self.location(frame))
            .collect::<Vec<_>>();
        if location_indices.is_empty() {
            return None;
        }
        if let Some(index) = self.stack_lookup.get(&location_indices) {
            return Some(*index);
        }

        let index = self.stacks.len() as i32;
        self.stacks.push(Stack {
            location_indices: location_indices.clone(),
        });
        self.stack_lookup.insert(location_indices, index);
        Some(index)
    }

    fn thread_attribute(&mut self, thread_name: &str) -> i32 {
        let key = self.intern("thread.name");
        let lookup_key = (key, thread_name.to_string());
        if let Some(index) = self.attribute_lookup.get(&lookup_key) {
            return *index;
        }

        let index = self.attributes.len() as i32;
        self.attributes.push(KeyValueAndUnit {
            key_strindex: key,
            value: Some(AnyValue {
                value: Some(any_value::Value::StringValue(thread_name.to_string())),
            }),
            unit_strindex: 0,
        });
        self.attribute_lookup.insert(lookup_key, index);
        index
    }

    fn build(self) -> ProfilesDictionary {
        ProfilesDictionary {
            mapping_table: vec![Mapping::default()],
            location_table: self.locations,
            function_table: self.functions,
            link_table: vec![Link::default()],
            string_table: self.strings,
            attribute_table: self.attributes,
            stack_table: self.stacks,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]

    use super::*;
    use prost::Message as _;

    fn symbol(name: &str, line: i64) -> ProfileSymbol {
        ProfileSymbol {
            name: name.to_string(),
            system_name: name.to_string(),
            filename: "src/main.rs".to_string(),
            line,
            address: 0x1000,
        }
    }

    fn sample(name: &str, count: i64, thread: &str) -> ProfileSample {
        ProfileSample {
            frames: vec![
                ProfileFrame {
                    symbols: vec![symbol(name, 10)],
                },
                ProfileFrame {
                    symbols: vec![symbol("main", 1)],
                },
            ],
            count,
            thread_name: Some(thread.to_string()),
        }
    }

    fn request() -> ExportProfilesServiceRequest {
        build_export_request(
            Resource::default(),
            collected(vec![
                sample("handle_query", 3, "worker-1"),
                sample("handle_query", 2, "worker-1"),
                sample("handle_query", 1, "worker-2"),
            ]),
        )
    }

    fn collected(samples: Vec<ProfileSample>) -> CollectedProfile {
        CollectedProfile {
            samples,
            time_unix_nano: 1_700_000_000_000_000_000,
            duration_nano: 5_000_000_000,
            frequency: 100,
        }
    }

    fn dictionary(request: &ExportProfilesServiceRequest) -> &ProfilesDictionary {
        request
            .dictionary
            .as_ref()
            .expect("dictionary should be present")
    }

    #[test]
    fn dictionary_tables_start_with_zero_values() {
        let request = request();
        let dictionary = dictionary(&request);

        assert_eq!(dictionary.string_table[0], "");
        assert_eq!(dictionary.mapping_table[0], Mapping::default());
        assert_eq!(dictionary.location_table[0], Location::default());
        assert_eq!(dictionary.function_table[0], Function::default());
        assert_eq!(dictionary.link_table[0], Link::default());
        assert_eq!(dictionary.stack_table[0], Stack::default());
        assert_eq!(dictionary.attribute_table[0], KeyValueAndUnit::default());
    }

    #[test]
    fn identical_stacks_are_deduplicated() {
        let request = request();
        let dictionary = dictionary(&request);
        let profile = &request.resource_profiles[0].scope_profiles[0].profiles[0];

        assert_eq!(profile.samples.len(), 3);
        // All three samples share the same stack; threads are attributes.
        assert_eq!(dictionary.stack_table.len(), 2);
        assert_eq!(
            profile.samples[0].stack_index,
            profile.samples[1].stack_index
        );
        assert_eq!(
            profile.samples[0].stack_index,
            profile.samples[2].stack_index
        );
        assert_ne!(
            profile.samples[0].attribute_indices,
            profile.samples[2].attribute_indices
        );
    }

    #[test]
    fn sample_values_are_count_times_period() {
        let request = request();
        let profile = &request.resource_profiles[0].scope_profiles[0].profiles[0];
        // frequency 100 => period 10ms = 10_000_000 ns
        assert_eq!(profile.samples[0].values, vec![30_000_000]);
        assert_eq!(profile.samples[1].values, vec![20_000_000]);
        assert_eq!(profile.samples[2].values, vec![10_000_000]);
        assert_eq!(profile.period, 10_000_000);
    }

    #[test]
    fn sample_type_is_cpu_nanoseconds() {
        let request = request();
        let dictionary = dictionary(&request);
        let profile = &request.resource_profiles[0].scope_profiles[0].profiles[0];
        let sample_type = profile.sample_type.as_ref().expect("sample_type");

        assert_eq!(
            dictionary.string_table[sample_type.type_strindex as usize],
            "cpu"
        );
        assert_eq!(
            dictionary.string_table[sample_type.unit_strindex as usize],
            "nanoseconds"
        );
    }

    #[test]
    fn thread_name_is_referenced_from_sample() {
        let request = request();
        let dictionary = dictionary(&request);
        let profile = &request.resource_profiles[0].scope_profiles[0].profiles[0];
        let attribute_index = profile.samples[0].attribute_indices[0] as usize;
        let attribute = &dictionary.attribute_table[attribute_index];

        assert_eq!(
            dictionary.string_table[attribute.key_strindex as usize],
            "thread.name"
        );
        match attribute
            .value
            .as_ref()
            .and_then(|value| value.value.as_ref())
        {
            Some(any_value::Value::StringValue(value)) => assert_eq!(value, "worker-1"),
            other => panic!("expected string thread attribute, got {other:?}"),
        }
    }

    #[test]
    fn request_is_valid_protobuf_and_has_service_scope() {
        let request = request();
        let bytes = request.encode_to_vec();
        let decoded = ExportProfilesServiceRequest::decode(bytes.as_slice()).unwrap();

        assert_eq!(decoded.resource_profiles.len(), 1);
        let scope = decoded.resource_profiles[0].scope_profiles[0]
            .scope
            .as_ref()
            .unwrap();
        assert_eq!(scope.name, "doggy-dns");
    }

    #[test]
    fn profile_id_is_non_zero_and_unique() {
        let first = request();
        let second = request();
        let first_id = &first.resource_profiles[0].scope_profiles[0].profiles[0].profile_id;
        let second_id = &second.resource_profiles[0].scope_profiles[0].profiles[0].profile_id;

        assert_eq!(first_id.len(), 16);
        assert_ne!(first_id, second_id);
        assert!(first_id.iter().any(|byte| *byte != 0));
    }

    #[test]
    fn empty_profile_still_encodes() {
        let request = build_export_request(Resource::default(), collected(Vec::new()));
        let profile = &request.resource_profiles[0].scope_profiles[0].profiles[0];
        assert!(profile.samples.is_empty());
        assert!(!request.encode_to_vec().is_empty());
    }

    #[test]
    fn resource_attributes_are_preserved() {
        let resource = Resource {
            attributes: vec![opentelemetry_proto::tonic::common::v1::KeyValue {
                key: "service.name".to_string(),
                value: Some(AnyValue {
                    value: Some(any_value::Value::StringValue("doggy-dns".to_string())),
                }),
                key_strindex: 0,
            }],
            dropped_attributes_count: 0,
            entity_refs: Vec::new(),
        };
        let request = build_export_request(resource, collected(vec![sample("f", 1, "t")]));
        let resource = request.resource_profiles[0]
            .resource
            .as_ref()
            .expect("resource should be present");

        assert_eq!(resource.attributes[0].key, "service.name");
    }
}
