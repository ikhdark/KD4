use base64::Engine;
use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;

#[test]
fn compose_success_url_uses_local_page_by_default() {
    let LoginSuccessRedirect::Local(url) = compose_success_url(
        /*port*/ 43123,
        DEFAULT_ISSUER,
        "e30.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnt9fQ.sig",
        "e30.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnt9fQ.sig",
        /*codex_streamlined_login*/ false,
        &LoginSuccessPage::default(),
    ) else {
        panic!("expected local success redirect");
    };
    let url = Url::parse(&url).expect("success URL should parse");

    assert_eq!(url.host_str(), Some("localhost"));
    assert_eq!(url.port(), Some(43123));
    assert_eq!(url.path(), "/success");
    assert!(!url.query_pairs().any(|(key, _)| key == "id_token"));
    assert_eq!(
        url.query_pairs()
            .find(|(key, _)| key == "codex_streamlined_login"),
        None
    );
}

#[test]
fn compose_success_url_uses_streamlined_local_page_when_requested() {
    let LoginSuccessRedirect::Local(url) = compose_success_url(
        /*port*/ 1455,
        DEFAULT_ISSUER,
        "e30.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnt9fQ.sig",
        "e30.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnt9fQ.sig",
        /*codex_streamlined_login*/ true,
        &LoginSuccessPage::default(),
    ) else {
        panic!("expected local success redirect");
    };
    let url = Url::parse(&url).expect("success URL should parse");

    assert_eq!(
        url.query_pairs()
            .find(|(key, _)| key == "codex_streamlined_login")
            .map(|(_, value)| value.into_owned()),
        Some("true".to_string())
    );
}

#[test]
fn compose_success_url_uses_hosted_page_when_requested() {
    assert_eq!(
        compose_success_url(
            /*port*/ 1455,
            DEFAULT_ISSUER,
            "e30.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnt9fQ.sig",
            "e30.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnt9fQ.sig",
            /*codex_streamlined_login*/ false,
            &LoginSuccessPage::Hosted {
                url: Url::parse("https://chatgpt.com/codex/open-app?id_token=secret&old=value")
                    .expect("open app URL should parse"),
                app_brand: LoginSuccessPageBrand::Chatgpt,
            },
        ),
        LoginSuccessRedirect::Hosted(
            "https://chatgpt.com/codex/open-app?source=login&app_brand=chatgpt".to_string()
        )
    );
}

#[test]
fn compose_success_url_keeps_setup_on_local_page() {
    let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let payload = encode(
        serde_json::to_string(&json!({
            "https://api.openai.com/auth": {
                "completed_platform_onboarding": false,
                "is_org_owner": true,
                "organization_id": "org_123",
                "project_id": "proj_123",
            }
        }))
        .expect("payload should serialize")
        .as_bytes(),
    );
    let access_payload = encode(
        serde_json::to_string(&json!({
            "https://api.openai.com/auth": {
                "chatgpt_plan_type": "team",
            }
        }))
        .expect("payload should serialize")
        .as_bytes(),
    );
    let id_token = format!("e30.{payload}.sig");
    let LoginSuccessRedirect::Local(url) = compose_success_url(
        /*port*/ 1455,
        DEFAULT_ISSUER,
        &id_token,
        &format!("e30.{access_payload}.sig"),
        /*codex_streamlined_login*/ true,
        &LoginSuccessPage::Hosted {
            url: Url::parse(CODEX_OPEN_APP_URL).expect("open app URL should parse"),
            app_brand: LoginSuccessPageBrand::Codex,
        },
    ) else {
        panic!("expected local success redirect");
    };
    let url = Url::parse(&url).expect("success URL should parse");

    assert_eq!(url.host_str(), Some("localhost"));
    assert_eq!(url.path(), "/success");
    assert_eq!(
        url.query_pairs()
            .find(|(key, _)| key == "needs_setup")
            .map(|(_, value)| value.into_owned()),
        Some("true".to_string())
    );
}

#[test]
fn issuer_trailing_slash_and_jwt_shape_preserve_setup_contract() {
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({
        "https://api.openai.com/auth": {"is_org_owner": true, "organization_id": "org", "project_id": "project", "chatgpt_plan_type": "pro"}
    })).unwrap());
    let jwt = format!("e30.{payload}.sig");
    let LoginSuccessRedirect::Local(url) = compose_success_url(
        1455,
        &format!("{DEFAULT_ISSUER}/"),
        &jwt,
        &jwt,
        false,
        &LoginSuccessPage::Local,
    ) else {
        panic!("expected local setup");
    };
    let url = Url::parse(&url).unwrap();
    let params: std::collections::HashMap<_, _> = url.query_pairs().collect();
    assert_eq!(params["platform_url"], "https://platform.openai.com");
    assert_eq!(params["needs_setup"], "true");
    assert_eq!(params["org_id"], "org");
    assert_eq!(params["project_id"], "project");
    assert_eq!(params["plan_type"], "pro");
    assert_eq!(params["id_token"], jwt);
    for invalid in [format!("{jwt}.extra"), format!("{jwt}."), "e30..sig".into()] {
        assert!(jwt_auth_claims(&invalid).is_empty());
    }
}
