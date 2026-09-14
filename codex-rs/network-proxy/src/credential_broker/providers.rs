mod github;
mod openai;

use rama_http::HeaderMap;
use rama_http::HeaderValue;
use rand::Rng as _;
use std::collections::HashMap;

const DUMMY_ALPHANUMERIC: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

type RequestHeader = for<'a> fn(&'a HeaderMap) -> Option<&'a HeaderValue>;

/// Describes how one credential family is recognized and injected.
///
/// Providers must be declared as `static` values because the broker uses their addresses as stable
/// identities when deduplicating credential records.
pub(super) struct CredentialProvider {
    context_env_vars: &'static [&'static str],
    sources: &'static [CredentialSource],
    dummy_value: fn(&str) -> String,
    request_header: RequestHeader,
    request_header_value: fn(&str) -> Option<HeaderValue>,
    insert_request_header: fn(&mut HeaderMap, HeaderValue),
}

#[derive(Clone, PartialEq, Eq)]
pub(super) enum CredentialHostBinding {
    ExactHost(String),
    HostPattern {
        exact_hosts: &'static [&'static str],
        suffixes: &'static [&'static str],
    },
}

pub(super) struct CredentialSource {
    pub(super) env_vars: &'static [&'static str],
    pub(super) host_binding: fn(&HashMap<String, String>) -> Option<CredentialHostBinding>,
}

const CREDENTIAL_PROVIDERS: &[&CredentialProvider] = &[&github::PROVIDER, &openai::PROVIDER];

impl CredentialProvider {
    pub(super) fn sources(&self) -> &[CredentialSource] {
        self.sources
    }

    pub(super) fn dummy_value(&self, real_value: &str) -> String {
        (self.dummy_value)(real_value)
    }

    pub(super) fn request_header<'a>(&self, headers: &'a HeaderMap) -> Option<&'a HeaderValue> {
        (self.request_header)(headers)
    }

    pub(super) fn request_matches_credential(&self, headers: &HeaderMap, credential: &str) -> bool {
        // Both current providers use Authorization. Reject duplicate fields rather than choosing
        // a credential whose interpretation could differ from the upstream server's.
        if headers
            .get_all(rama_http::header::AUTHORIZATION)
            .iter()
            .count()
            != 1
        {
            return false;
        }
        let Some(value) = self
            .request_header(headers)
            .and_then(|value| value.to_str().ok())
        else {
            return false;
        };
        let Some((scheme, value)) = value.split_once(' ') else {
            return false;
        };
        let valid_scheme = scheme.eq_ignore_ascii_case("bearer")
            || (std::ptr::eq(self, &github::PROVIDER) && scheme.eq_ignore_ascii_case("token"));
        valid_scheme && value.trim_matches(' ') == credential
    }

    pub(super) fn request_header_value(&self, value: &str) -> Option<HeaderValue> {
        let mut header = (self.request_header_value)(value)?;
        header.set_sensitive(true);
        Some(header)
    }

    pub(super) fn insert_request_header(&self, headers: &mut HeaderMap, value: HeaderValue) {
        (self.insert_request_header)(headers, value);
    }
}

impl CredentialHostBinding {
    pub(super) fn matches_host(&self, host: &str) -> bool {
        match self {
            Self::ExactHost(expected_host) => host == expected_host,
            Self::HostPattern {
                exact_hosts,
                suffixes,
            } => {
                exact_hosts.contains(&host) || suffixes.iter().any(|suffix| host.ends_with(suffix))
            }
        }
    }
}

pub(super) fn credential_broker_env_keys() -> impl Iterator<Item = &'static str> {
    credential_providers()
        .flat_map(|provider| provider.context_env_vars.iter().copied())
        .chain(
            credential_providers()
                .flat_map(CredentialProvider::sources)
                .flat_map(|source| source.env_vars.iter().copied()),
        )
}

pub(super) fn credential_providers() -> impl Iterator<Item = &'static CredentialProvider> {
    CREDENTIAL_PROVIDERS.iter().copied()
}

fn shaped_dummy_value(real_value: &str, prefix: &str, minimum_len: usize) -> String {
    let target_len = real_value.len().max(minimum_len).max(prefix.len() + 16);
    // Keep supported separators only when the remaining positions provide enough randomness.
    // Otherwise replace the entire suffix, including unsupported UTF-8 bytes.
    let preserve_shape = real_value.is_ascii()
        && real_value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        && (prefix.len()..target_len)
            .filter(|&index| {
                real_value
                    .as_bytes()
                    .get(index)
                    .is_none_or(u8::is_ascii_alphanumeric)
            })
            .count()
            >= 16;
    let mut rng = rand::rng();
    let mut dummy = String::with_capacity(target_len);
    dummy.push_str(prefix);
    for index in prefix.len()..target_len {
        let character = match real_value.as_bytes().get(index).copied() {
            Some(template) if preserve_shape && matches!(template, b'-' | b'_') => template,
            _ => DUMMY_ALPHANUMERIC[rng.random_range(0..DUMMY_ALPHANUMERIC.len())],
        };
        dummy.push(char::from(character));
    }
    dummy
}
