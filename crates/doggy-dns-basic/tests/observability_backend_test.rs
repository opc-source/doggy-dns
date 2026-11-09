#![allow(clippy::disallowed_methods)]

use doggy_dns_basic::ObservabilityBackend;

#[test]
fn vector_is_the_only_backend_without_profile_support() {
    assert!(!ObservabilityBackend::Vector.supports_profiles());
    for backend in [
        ObservabilityBackend::OpenObserve,
        ObservabilityBackend::OtelCollector,
        ObservabilityBackend::Custom,
    ] {
        assert!(
            backend.supports_profiles(),
            "{backend:?} should support profiles"
        );
    }
}
