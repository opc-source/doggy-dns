#![allow(clippy::disallowed_methods)]

use doggy_dns_basic::validate_otlp_base_url;

#[test]
fn accepts_a_base_url_with_a_trailing_slash() {
    assert!(validate_otlp_base_url("observability.endpoint", "http://collector:4318/").is_ok());
}

#[test]
fn rejects_urls_that_already_carry_a_signal_path() {
    for value in [
        "http://collector:4318/v1",
        "http://collector:4318/v1/",
        "http://collector:4318/v1/logs",
        "http://collector:4318/v1/profiles/",
    ] {
        assert!(
            validate_otlp_base_url("observability.endpoint", value).is_err(),
            "{value} must be rejected"
        );
    }
}

#[test]
fn trims_surrounding_whitespace_before_checking() {
    assert!(validate_otlp_base_url("observability.endpoint", " http://collector:4318/ ").is_ok());
    assert!(
        validate_otlp_base_url("observability.endpoint", " http://collector:4318/v1/logs ")
            .is_err()
    );
}

#[test]
fn rejects_query_strings_and_fragments() {
    assert!(validate_otlp_base_url("observability.endpoint", "http://collector:4318?x=1").is_err());
    assert!(
        validate_otlp_base_url("observability.endpoint", "http://collector:4318#frag").is_err()
    );
}

#[test]
fn the_caller_supplies_the_name_in_the_error() {
    let error = validate_otlp_base_url("OTLP base endpoint", "http://collector:4318/v1/logs")
        .unwrap_err()
        .to_string();
    assert!(error.contains("OTLP base endpoint"));
}
