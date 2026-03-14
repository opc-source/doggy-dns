use dns_filter_core::middleware::metrics::MetricsMiddleware;

#[test]
fn metrics_counter_starts_at_zero() {
    let metrics = MetricsMiddleware::new();
    assert_eq!(metrics.total_queries(), 0);
}
