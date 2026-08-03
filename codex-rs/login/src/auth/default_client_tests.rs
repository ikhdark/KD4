use super::sanitize_user_agent;
use super::*;
use core_test_support::skip_if_no_network;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::Barrier;
use std::sync::RwLock;
use std::thread;

#[test]
fn test_get_codex_user_agent() {
    let user_agent = get_codex_user_agent();
    let originator = originator().value;
    let prefix = format!("{originator}/");
    assert!(user_agent.starts_with(&prefix));
}

#[test]
fn is_first_party_originator_matches_known_values() {
    assert_eq!(is_first_party_originator(DEFAULT_ORIGINATOR), true);
    assert_eq!(is_first_party_originator("codex-tui"), true);
    assert_eq!(is_first_party_originator("codex_vscode"), true);
    assert_eq!(is_first_party_originator("Codex Something Else"), true);
    assert_eq!(is_first_party_originator("codex_cli"), false);
    assert_eq!(is_first_party_originator("Other"), false);
}

#[test]
fn is_first_party_chat_originator_matches_known_values() {
    assert_eq!(is_first_party_chat_originator("codex_atlas"), true);
    assert_eq!(
        is_first_party_chat_originator("codex_chatgpt_desktop"),
        true
    );
    assert_eq!(is_first_party_chat_originator(DEFAULT_ORIGINATOR), false);
    assert_eq!(is_first_party_chat_originator("codex_vscode"), false);
}

#[tokio::test]
async fn test_create_client_sets_default_headers() {
    skip_if_no_network!();

    set_default_client_residency_requirement(Some(ResidencyRequirement::Us));

    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    let client = create_client();

    // Spin up a local mock server and capture a request.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let resp = client
        .get(server.uri())
        .send()
        .await
        .expect("failed to send request");
    assert!(resp.status().is_success());

    let requests = server
        .received_requests()
        .await
        .expect("failed to fetch received requests");
    assert!(!requests.is_empty());
    let headers = &requests[0].headers;

    // originator header is set to the provided value
    let originator_header = headers
        .get("originator")
        .expect("originator header missing");
    assert_eq!(originator_header.to_str().unwrap(), originator().value);

    // User-Agent matches the computed Codex UA for that originator
    let expected_ua = get_codex_user_agent();
    let ua_header = headers
        .get("user-agent")
        .expect("user-agent header missing");
    assert_eq!(ua_header.to_str().unwrap(), expected_ua);

    let residency_header = headers
        .get(RESIDENCY_HEADER_NAME)
        .expect("residency header missing");
    assert_eq!(residency_header.to_str().unwrap(), "us");

    set_default_client_residency_requirement(/*enforce_residency*/ None);
}

#[test]
fn test_invalid_suffix_is_sanitized() {
    let prefix = "codex_cli_rs/0.0.0";
    let suffix = "bad\rsuffix";

    assert_eq!(
        sanitize_user_agent(format!("{prefix} ({suffix})"), prefix),
        "codex_cli_rs/0.0.0 (bad_suffix)"
    );
}

#[test]
fn test_invalid_suffix_is_sanitized2() {
    let prefix = "codex_cli_rs/0.0.0";
    let suffix = "bad\0suffix";

    assert_eq!(
        sanitize_user_agent(format!("{prefix} ({suffix})"), prefix),
        "codex_cli_rs/0.0.0 (bad_suffix)"
    );
}

#[test]
fn concurrent_process_identity_installation_never_exposes_a_mixed_user_agent() {
    let state = Arc::new(RwLock::new(ProcessIdentityState::default()));
    let barrier = Arc::new(Barrier::new(4));

    let spawn_writer = |originator: &'static str, suffix: &'static str| {
        let state = Arc::clone(&state);
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            barrier.wait();
            install_process_identity(
                &state,
                originator.to_string(),
                Some(Some(suffix.to_string())),
                || None,
            )
        })
    };
    let writer_a = spawn_writer("client_a", "client_a; 1.0.0");
    let writer_b = spawn_writer("client_b", "client_b; 2.0.0");

    let reader_state = Arc::clone(&state);
    let reader_barrier = Arc::clone(&barrier);
    let reader = thread::spawn(move || {
        reader_barrier.wait();
        (0..2_000)
            .map(|_| {
                let snapshot = process_identity_snapshot_from_state(&reader_state, || None);
                thread::yield_now();
                codex_user_agent_for_identity(&snapshot)
            })
            .collect::<Vec<_>>()
    });

    barrier.wait();
    let results = [
        writer_a.join().expect("client_a writer should not panic"),
        writer_b.join().expect("client_b writer should not panic"),
    ];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);

    for user_agent in reader.join().expect("identity reader should not panic") {
        let is_unclaimed = user_agent.starts_with(&format!("{DEFAULT_ORIGINATOR}/"))
            && !user_agent.ends_with(" (client_a; 1.0.0)")
            && !user_agent.ends_with(" (client_b; 2.0.0)");
        let is_client_a =
            user_agent.starts_with("client_a/") && user_agent.ends_with(" (client_a; 1.0.0)");
        let is_client_b =
            user_agent.starts_with("client_b/") && user_agent.ends_with(" (client_b; 2.0.0)");
        assert!(
            is_unclaimed || is_client_a || is_client_b,
            "observed mixed process identity: {user_agent}"
        );
    }

    let final_user_agent =
        codex_user_agent_for_identity(&process_identity_snapshot_from_state(&state, || None));
    assert!(
        (final_user_agent.starts_with("client_a/")
            && final_user_agent.ends_with(" (client_a; 1.0.0)"))
            || (final_user_agent.starts_with("client_b/")
                && final_user_agent.ends_with(" (client_b; 2.0.0)"))
    );
}

#[test]
fn process_originator_override_discards_request_client_suffix() {
    let state = RwLock::new(ProcessIdentityState::default());
    install_process_identity(
        &state,
        "request_client".to_string(),
        Some(Some("request_client; 1.0.0".to_string())),
        || Some("process_override".to_string()),
    )
    .expect("override identity should install");

    let snapshot = process_identity_snapshot_from_state(&state, || None);
    assert_eq!(snapshot.originator.value, "process_override");
    assert_eq!(snapshot.suffix, None);
    let user_agent = codex_user_agent_for_identity(&snapshot);
    assert!(user_agent.starts_with("process_override/"));
    assert!(!user_agent.contains("request_client"));
}

#[test]
#[cfg(target_os = "macos")]
fn test_macos() {
    use regex_lite::Regex;
    let user_agent = get_codex_user_agent();
    let originator = regex_lite::escape(originator().value.as_str());
    let re = Regex::new(&format!(
        r"^{originator}/\d+\.\d+\.\d+ \(Mac OS \d+\.\d+\.\d+; (x86_64|arm64)\) (\S+)$"
    ))
    .unwrap();
    assert!(re.is_match(&user_agent));
}
