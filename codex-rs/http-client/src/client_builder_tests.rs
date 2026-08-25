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

#[test]
fn async_builder_accepts_transport_neutral_tls_material() {
    let ca_pem = include_bytes!("../tests/fixtures/test-ca.pem");

    let client = HttpClientBuilder::new()
        .timeout(std::time::Duration::from_secs(1))
        .tls_certs_only_pem(ca_pem)
        .expect("valid CA certificate")
        .https_only(true)
        .build_direct();

    assert!(client.is_ok());
}
