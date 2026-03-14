use dns_filter_plugin::authority_chain::AuthorityChain;
use hickory_proto::rr::{Name, RecordType};
use hickory_server::zone_handler::LookupControlFlow;
use std::str::FromStr;

#[tokio::test]
async fn empty_chain_returns_skip() {
    let chain = AuthorityChain::new(vec![]);
    let name = Name::from_str("example.com.").unwrap();
    let result = chain.resolve(&name, RecordType::A, None).await;
    assert!(matches!(result, LookupControlFlow::Skip));
}
