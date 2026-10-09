use super::*;
use pretty_assertions::assert_eq;

#[test]
fn callback_state_requires_matching_nonce_and_allowlisted_suffix() {
    for (callback_state, expected) in [
        ("expected-state", Some(LoginCallbackResult::default())),
        (
            "expected-state.onboarding_entrypoint=life_sciences",
            Some(LoginCallbackResult {
                onboarding_entrypoint: Some(LoginOnboardingEntrypoint::LifeSciences),
            }),
        ),
        ("different-state.onboarding_entrypoint=life_sciences", None),
        ("expected-state.onboarding_entrypoint=unknown", None),
        ("expected-state.onboarding_entrypoint=life_sciences.onboarding_entrypoint=life_sciences", None),
        ("expected-state.extra=value.onboarding_entrypoint=life_sciences", None),
    ] {
        assert_eq!(
            login_callback_result_from_state(callback_state, "expected-state"),
            expected,
            "{callback_state}",
        );
    }
}
