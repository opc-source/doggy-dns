#![allow(clippy::disallowed_methods)]

use doggy_dns_plugin::builtin::wire_query_name;
use hickory_proto::rr::{IntoName, LowerName, Name};

/// A Nacos-shaped name as it arrives on the wire: service, group, namespace,
/// zone, with the interior underscore of `DEFAULT_GROUP` already in the bytes.
const WIRE_NAME: &str = "web-app.DEFAULT_GROUP.public.nacos.local.";

fn labels(name: &Name) -> Vec<String> {
    name.iter()
        .map(|label| String::from_utf8_lossy(label).into_owned())
        .collect()
}

#[test]
fn wire_query_name_keeps_underscore_labels_intact() {
    let wire = Name::from_ascii(WIRE_NAME).expect("underscore labels parse from the wire");
    let query = wire_query_name(&LowerName::from(wire));

    assert_eq!(
        labels(&query),
        vec!["web-app", "default_group", "public", "nacos", "local"],
        "every label must reach the resolver unchanged, only case-folded"
    );
    assert!(query.is_fqdn(), "the trailing dot must not be dropped");
}

#[test]
fn string_built_names_are_what_reject_underscores() {
    // Guards the premise of `wire_query_name`: should hickory ever start
    // accepting interior underscores on the `&str` path, the helper's comment
    // and its two call sites deserve a revisit instead of quietly rotting.
    let error = "web-app.default_group.public.nacos.local."
        .into_name()
        .expect_err("STD3 rejects interior underscores on the &str path");
    assert!(
        error
            .to_string()
            .contains("Label contains invalid characters"),
        "unexpected failure for a string-built name: {error}"
    );

    // A leading underscore is the documented SRV exemption and must keep working.
    assert!("_svc._tcp.example.com.".into_name().is_ok());
}
