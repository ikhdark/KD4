use rama_tls_rustls::dep::pki_types::CertificateDer;
use rustls_native_certs::CertificateResult;

use rustls_native_certs::Error;

use rustls_native_certs::ErrorKind;

// `rustls_native_certs::load_native_certs()` first consults SSL_CERT_FILE and
// SSL_CERT_DIR. Load platform roots directly so a startup custom CA can be
// layered onto the managed bundle without replacing the platform trust store.

pub(crate) fn load_platform_native_certs() -> CertificateResult {
    use schannel::cert_store::CertStore;

    let mut result = CertificateResult::default();
    let current_user_store = match CertStore::open_current_user("ROOT") {
        Ok(store) => store,
        Err(err) => {
            result.errors.push(Error {
                context: "failed to open current user certificate store",
                kind: ErrorKind::Os(err.into()),
            });
            return result;
        }
    };

    for cert in current_user_store.certs() {
        let valid_uses = match cert.valid_uses() {
            Ok(valid_uses) => valid_uses,
            Err(err) => {
                result.errors.push(Error {
                    context: "failed to inspect certificate valid uses",
                    kind: ErrorKind::Os(err.into()),
                });
                continue;
            }
        };
        let is_time_valid = match cert.is_time_valid() {
            Ok(is_time_valid) => is_time_valid,
            Err(err) => {
                result.errors.push(Error {
                    context: "failed to inspect certificate time validity",
                    kind: ErrorKind::Os(err.into()),
                });
                continue;
            }
        };
        if usable_for_rustls(valid_uses) && is_time_valid {
            result
                .certs
                .push(CertificateDer::from(cert.to_der().to_vec()));
        }
    }
    result
}

fn usable_for_rustls(uses: schannel::cert_context::ValidUses) -> bool {
    match uses {
        schannel::cert_context::ValidUses::All => true,
        schannel::cert_context::ValidUses::Oids(strs) => strs.iter().any(|x| x == PKIX_SERVER_AUTH),
    }
}

const PKIX_SERVER_AUTH: &str = "1.3.6.1.5.5.7.3.1";
