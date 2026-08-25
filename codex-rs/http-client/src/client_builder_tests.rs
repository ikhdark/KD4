use super::*;
use std::path::PathBuf;

#[test]
fn completed_custom_ca_migration_has_no_fallback_terminal() {
    let source = include_str!("client_builder.rs");

    assert!(
        !source.contains("custom_ca_fallback"),
        "the completed custom-CA migration must not retain a fallback escape hatch"
    );
}

#[test]
fn transport_default_client_propagates_custom_ca_failure() {
    let error = HttpClientBuilder::new()
        .build_with_transport_default_proxy_using(|_| {
            Err(BuildCustomCaTransportError::InvalidCaFile {
                source_env: "TEST_CA_ENV",
                path: PathBuf::from("invalid-test-ca.pem"),
                detail: "synthetic invalid CA".to_string(),
            })
        })
        .expect_err("invalid custom CA must fail client construction");

    assert!(matches!(
        error,
        BuildCustomCaTransportError::InvalidCaFile {
            source_env: "TEST_CA_ENV",
            ..
        }
    ));
}
