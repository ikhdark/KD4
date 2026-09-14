use super::*;
use std::path::PathBuf;

#[test]
fn transport_default_client_propagates_custom_ca_failure() {
    let error = HttpClientBuilder::new()
        .build_with_transport_default_proxy_using(|_, _| {
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
fn exclusive_tls_roots_select_explicit_root_policy() {
    let ca_pem = include_bytes!("../tests/fixtures/test-ca.pem");

    let client = HttpClientBuilder::new()
        .tls_certs_only_pem(ca_pem)
        .expect("valid CA certificate")
        .build_with_transport_default_proxy_using(|builder, custom_ca_policy| {
            assert_eq!(custom_ca_policy, CustomCaPolicy::ExplicitRootSet);
            builder
                .build()
                .map_err(BuildCustomCaTransportError::BuildClientWithExplicitRoots)
        });

    assert!(client.is_ok());
}

#[tokio::test]
async fn async_builder_enforces_https_with_transport_neutral_tls_material() {
    let ca_pem = include_bytes!("../tests/fixtures/test-ca.pem");

    let client = HttpClientBuilder::new()
        .timeout(std::time::Duration::from_secs(1))
        .tls_certs_only_pem(ca_pem)
        .expect("valid CA certificate")
        .https_only(true)
        .build_direct()
        .expect("build client");

    let error = client
        .get("http://127.0.0.1:1/")
        .send()
        .await
        .expect_err("HTTPS-only rejects HTTP");
    assert!(error.is_builder());
}
