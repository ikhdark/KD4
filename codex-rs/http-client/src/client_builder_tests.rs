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
    for pem in [
        b"".as_slice(),
        b"not a certificate".as_slice(),
        b"-----BEGIN CERTIFICATE-----\n!\n-----END CERTIFICATE-----\n".as_slice(),
    ] {
        let error = HttpClientBuilder::new()
            .tls_certs_only_pem(pem)
            .err()
            .expect("empty or malformed explicit roots must fail");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    let invalid_der = b"-----BEGIN CERTIFICATE-----\nMAA=\n-----END CERTIFICATE-----\n";
    assert!(
        HttpClientBuilder::new()
            .tls_certs_only_pem(invalid_der)
            .expect("well-formed PEM is parsed before DER validation")
            .build_direct()
            .is_err(),
        "invalid DER must fail client construction"
    );

    let ca_pem = include_bytes!("../tests/fixtures/test-ca.pem");
    for count in [1, 2] {
        let builder = HttpClientBuilder::new()
            .tls_certs_only_pem(&ca_pem.repeat(count))
            .expect("valid CA certificate bundle");
        assert_eq!(builder.tls_certs_only.as_ref().unwrap().len(), count);
        let client = builder
            .build_with_transport_default_proxy_using(|builder, custom_ca_policy| {
                assert_eq!(custom_ca_policy, CustomCaPolicy::ExplicitRootSet);
                builder
                    .build()
                    .map_err(BuildCustomCaTransportError::BuildClientWithExplicitRoots)
            });
        assert!(client.is_ok());
    }
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
