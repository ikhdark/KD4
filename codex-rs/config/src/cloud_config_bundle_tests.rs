use super::*;
use crate::ConfigLayerSource;
use crate::ConfigRequirementsToml;
use crate::compose_requirements;
use codex_protocol::protocol::AskForApproval;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tempfile::tempdir;

#[tokio::test]
async fn shared_future_runs_once() {
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = Arc::clone(&counter);
    let loader = CloudConfigBundleLoader::new(async move {
        counter_clone.fetch_add(1, Ordering::SeqCst);
        Ok(Some(CloudConfigBundle::default()))
    });

    let (first, second) = tokio::join!(loader.get(), loader.get());
    assert_eq!(first, second);
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn retryable_loader_shares_attempts_and_keeps_the_successful_snapshot() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let loader = CloudConfigBundleLoader::retryable({
        let attempts = Arc::clone(&attempts);
        move || {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            async move {
                tokio::task::yield_now().await;
                if attempt == 0 {
                    Err(CloudConfigBundleLoadError::new(
                        CloudConfigBundleLoadErrorCode::Timeout,
                        None,
                        "transient",
                    ))
                } else {
                    Ok(Some(CloudConfigBundle::default()))
                }
            }
        }
    });
    let (first, second) = tokio::join!(loader.get(), loader.get());
    assert_eq!(
        first.as_ref().unwrap_err().code(),
        CloudConfigBundleLoadErrorCode::Timeout
    );
    assert_eq!(first, second);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    let (first, second) = tokio::join!(loader.get(), loader.get());
    assert_eq!(first, Ok(Some(CloudConfigBundle::default())));
    assert_eq!(first, second);
    assert_eq!(loader.get().await, first);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn retryable_loader_never_retries_permanent_errors_or_drops_requirements() {
    for (code, status) in [
        (CloudConfigBundleLoadErrorCode::Auth, None),
        (CloudConfigBundleLoadErrorCode::InvalidBundle, None),
        (CloudConfigBundleLoadErrorCode::Internal, None),
        (CloudConfigBundleLoadErrorCode::RequestFailed, Some(403)),
    ] {
        let attempts = Arc::new(AtomicUsize::new(0));
        let loader = CloudConfigBundleLoader::retryable({
            let attempts = Arc::clone(&attempts);
            move || {
                attempts.fetch_add(1, Ordering::SeqCst);
                async move {
                    Err(CloudConfigBundleLoadError::new(
                        code,
                        status,
                        "required policy unavailable",
                    ))
                }
            }
        });
        assert_eq!(loader.get().await.unwrap_err().code(), code);
        assert_eq!(loader.get().await.unwrap_err().code(), code);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn retryable_loader_retains_inflight_attempt_when_waiter_is_cancelled() {
    use std::task::Context;
    let attempts = Arc::new(AtomicUsize::new(0));
    let ready = Arc::new(tokio::sync::Notify::new());
    let loader = CloudConfigBundleLoader::retryable({
        let attempts = Arc::clone(&attempts);
        let ready = Arc::clone(&ready);
        move || {
            attempts.fetch_add(1, Ordering::SeqCst);
            let ready = Arc::clone(&ready);
            async move {
                ready.notified().await;
                Ok(None)
            }
        }
    });
    let mut waiting = Box::pin(loader.get());
    assert!(
        waiting
            .as_mut()
            .poll(&mut Context::from_waker(std::task::Waker::noop()))
            .is_pending()
    );
    drop(waiting);
    ready.notify_one();
    assert_eq!(loader.get().await, Ok(None));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn retryable_loader_reopens_only_eligible_http_failures() {
    for (status, expected_attempts) in [
        (None, 2),
        (Some(408), 2),
        (Some(429), 2),
        (Some(503), 2),
        (Some(400), 1),
        (Some(600), 1),
    ] {
        let attempts = Arc::new(AtomicUsize::new(0));
        let loader = CloudConfigBundleLoader::retryable({
            let attempts = Arc::clone(&attempts);
            move || {
                attempts.fetch_add(1, Ordering::SeqCst);
                async move {
                    Err(CloudConfigBundleLoadError::new(
                        CloudConfigBundleLoadErrorCode::RequestFailed,
                        status,
                        "request failed",
                    ))
                }
            }
        });
        for _ in 0..2 {
            let error = loader.get().await.unwrap_err();
            assert_eq!(error.code(), CloudConfigBundleLoadErrorCode::RequestFailed);
            assert_eq!(error.status_code(), status);
        }
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            expected_attempts,
            "{status:?}"
        );
    }
}

#[test]
fn bundle_layers_preserve_enterprise_managed_bucket_order() {
    let tempdir = tempdir().expect("tempdir");
    let base_dir = AbsolutePathBuf::from_absolute_path(tempdir.path()).expect("absolute path");
    let layers = CloudConfigBundleLayers::from_bundle(
        CloudConfigBundle {
            config_toml: CloudConfigTomlBundle {
                enterprise_managed: vec![
                    CloudConfigFragment {
                        id: "cfg_high".to_string(),
                        name: "High config".to_string(),
                        contents: "model = \"high\"".to_string(),
                    },
                    CloudConfigFragment {
                        id: "cfg_low".to_string(),
                        name: "Low config".to_string(),
                        contents: "model = \"low\"".to_string(),
                    },
                ],
            },
            requirements_toml: CloudRequirementsTomlBundle {
                enterprise_managed: vec![
                    CloudRequirementsFragment {
                        id: "req_high".to_string(),
                        name: "High requirements".to_string(),
                        contents: "allowed_approval_policies = [\"on-request\"]".to_string(),
                    },
                    CloudRequirementsFragment {
                        id: "req_low".to_string(),
                        name: "Low requirements".to_string(),
                        contents: "allowed_approval_policies = [\"never\"]".to_string(),
                    },
                ],
            },
        },
        &base_dir,
    )
    .expect("bundle should be converted into layers");

    assert_eq!(
        layers
            .enterprise_managed_config
            .iter()
            .map(|layer| layer.name.clone())
            .collect::<Vec<_>>(),
        vec![
            ConfigLayerSource::EnterpriseManaged {
                id: "cfg_low".to_string(),
                name: "Low config".to_string(),
            },
            ConfigLayerSource::EnterpriseManaged {
                id: "cfg_high".to_string(),
                name: "High config".to_string(),
            },
        ]
    );
    assert_eq!(
        compose_requirements(layers.enterprise_managed_requirements)
            .expect("requirements should compose")
            .expect("requirements should be present")
            .into_toml(),
        ConfigRequirementsToml {
            allowed_approval_policies: Some(vec![AskForApproval::OnRequest]),
            ..Default::default()
        }
    );
}

#[test]
fn bundle_layers_can_strict_validate_enterprise_managed_config() {
    let tempdir = tempdir().expect("tempdir");
    let base_dir = AbsolutePathBuf::from_absolute_path(tempdir.path()).expect("absolute path");
    let err = CloudConfigBundleLayers::from_bundle_strict_config(
        CloudConfigBundle {
            config_toml: CloudConfigTomlBundle {
                enterprise_managed: vec![CloudConfigFragment {
                    id: "cfg".to_string(),
                    name: "Cloud config".to_string(),
                    contents: "unknown_key = true".to_string(),
                }],
            },
            requirements_toml: CloudRequirementsTomlBundle {
                enterprise_managed: Vec::new(),
            },
        },
        &base_dir,
    )
    .expect_err("strict config should reject unknown fields");

    assert_eq!(
        err,
        CloudConfigLayerError::Invalid {
            fragment: crate::CloudConfigFragmentSource {
                id: "cfg".to_string(),
                name: "Cloud config".to_string(),
            },
            message: "unknown configuration field `unknown_key`".to_string(),
        }
    );
}
