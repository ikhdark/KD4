use codex_http_client::build_reqwest_client_with_custom_ca;
use reqwest::header::CONTENT_TYPE;
use reqwest::header::HeaderMap;
use std::collections::HashMap;
use tracing::info;
use tracing::warn;

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct CodeEnvironment {
    id: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    is_pinned: Option<bool>,
    #[serde(default)]
    task_count: Option<i64>,
}

#[derive(Debug, PartialEq, Eq)]
enum EnvironmentListDiagnostic {
    Pretty(String),
    Raw(String),
}

#[derive(Debug, Clone)]
pub struct AutodetectSelection {
    pub id: String,
    pub label: Option<String>,
}

pub async fn autodetect_environment_id(
    base_url: &str,
    headers: &HeaderMap,
    desired_label: Option<String>,
) -> anyhow::Result<AutodetectSelection> {
    // 1) Try repo-specific environments based on local git origins (GitHub only, like VSCode)
    let origins = get_git_origins();
    crate::append_error_log(format!("env: git origins: {origins:?}"));
    let mut by_repo_envs: Vec<CodeEnvironment> = Vec::new();
    for origin in &origins {
        if let Some((owner, repo)) = parse_owner_repo(origin) {
            let url = if base_url.contains("/backend-api") {
                format!(
                    "{}/wham/environments/by-repo/{}/{}/{}",
                    base_url, "github", owner, repo
                )
            } else {
                format!(
                    "{}/api/codex/environments/by-repo/{}/{}/{}",
                    base_url, "github", owner, repo
                )
            };
            crate::append_error_log(format!("env: GET {url}"));
            match get_json::<Vec<CodeEnvironment>>(&url, headers).await {
                Ok(mut list) => {
                    crate::append_error_log(format!(
                        "env: by-repo returned {} env(s) for {owner}/{repo}",
                        list.len(),
                    ));
                    by_repo_envs.append(&mut list);
                }
                Err(e) => crate::append_error_log(format!(
                    "env: by-repo fetch failed for {owner}/{repo}: {e}"
                )),
            }
        }
    }
    if let Some(env) = pick_environment_row(&by_repo_envs, desired_label.as_deref()) {
        return Ok(AutodetectSelection {
            id: env.id.clone(),
            label: env.label.as_deref().map(str::to_owned),
        });
    }

    // 2) Fallback to the full list
    let list_url = if base_url.contains("/backend-api") {
        format!("{base_url}/wham/environments")
    } else {
        format!("{base_url}/api/codex/environments")
    };
    crate::append_error_log(format!("env: GET {list_url}"));
    // Fetch and log the full environments JSON for debugging
    let http = build_reqwest_client_with_custom_ca(reqwest::Client::builder())?;
    let res = http.get(&list_url).headers(headers.clone()).send().await?;
    let status = res.status();
    let ct = res
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = res.text().await.unwrap_or_default();
    crate::append_error_log(format!("env: status={status} content-type={ct}"));
    let (all_envs, diagnostic) = decode_environment_list_response(&list_url, status, &ct, &body);
    log_environment_list_diagnostic(diagnostic);
    let all_envs = all_envs?;
    if let Some(env) = pick_environment_row(&all_envs, desired_label.as_deref()) {
        return Ok(AutodetectSelection {
            id: env.id.clone(),
            label: env.label.as_deref().map(str::to_owned),
        });
    }
    anyhow::bail!("no environments available")
}

fn decode_environment_list_response(
    list_url: &str,
    status: reqwest::StatusCode,
    content_type: &str,
    body: &str,
) -> (
    anyhow::Result<Vec<CodeEnvironment>>,
    EnvironmentListDiagnostic,
) {
    if !status.is_success() {
        let diagnostic = match serde_json::from_str::<serde_json::Value>(body) {
            Ok(value) => EnvironmentListDiagnostic::Pretty(
                serde_json::to_string_pretty(&value).unwrap_or_else(|_| body.to_string()),
            ),
            Err(_) => EnvironmentListDiagnostic::Raw(body.to_string()),
        };
        return (
            Err(anyhow::anyhow!(
                "GET {list_url} failed: {status}; content-type={content_type}; body={body}"
            )),
            diagnostic,
        );
    }

    match serde_json::from_str::<Vec<CodeEnvironment>>(body) {
        Ok(environments) => {
            let diagnostic = EnvironmentListDiagnostic::Pretty(
                serde_json::to_string_pretty(&environments).unwrap_or_else(|_| body.to_string()),
            );
            (Ok(environments), diagnostic)
        }
        Err(error) => (
            Err(anyhow::anyhow!(
                "Decode error for {list_url}: {error}; content-type={content_type}; body={body}"
            )),
            EnvironmentListDiagnostic::Raw(body.to_string()),
        ),
    }
}

fn log_environment_list_diagnostic(diagnostic: EnvironmentListDiagnostic) {
    match diagnostic {
        EnvironmentListDiagnostic::Pretty(pretty) => {
            crate::append_error_log(format!("env: /environments JSON (pretty):\n{pretty}"));
        }
        EnvironmentListDiagnostic::Raw(raw) => {
            crate::append_error_log(format!("env: /environments (raw):\n{raw}"));
        }
    }
}

fn pick_environment_row(
    envs: &[CodeEnvironment],
    desired_label: Option<&str>,
) -> Option<CodeEnvironment> {
    if envs.is_empty() {
        return None;
    }
    if let Some(label) = desired_label {
        let lc = label.to_lowercase();
        if let Some(e) = envs
            .iter()
            .find(|e| e.label.as_deref().unwrap_or("").to_lowercase() == lc)
        {
            crate::append_error_log(format!("env: matched by label: {label} -> {}", e.id));
            return Some(e.clone());
        }
    }
    if envs.len() == 1 {
        crate::append_error_log("env: single environment available; selecting it");
        return Some(envs[0].clone());
    }
    if let Some(e) = envs.iter().find(|e| e.is_pinned.unwrap_or(false)) {
        crate::append_error_log(format!("env: selecting pinned environment: {}", e.id));
        return Some(e.clone());
    }
    // Highest task_count as heuristic
    if let Some(e) = envs
        .iter()
        .max_by_key(|e| e.task_count.unwrap_or(0))
        .or_else(|| envs.first())
    {
        crate::append_error_log(format!("env: selecting by task_count/first: {}", e.id));
        return Some(e.clone());
    }
    None
}

async fn get_json<T: serde::de::DeserializeOwned>(
    url: &str,
    headers: &HeaderMap,
) -> anyhow::Result<T> {
    let http = build_reqwest_client_with_custom_ca(reqwest::Client::builder())?;
    let res = http.get(url).headers(headers.clone()).send().await?;
    let status = res.status();
    let ct = res
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = res.text().await.unwrap_or_default();
    crate::append_error_log(format!("env: status={status} content-type={ct}"));
    if !status.is_success() {
        anyhow::bail!("GET {url} failed: {status}; content-type={ct}; body={body}");
    }
    let parsed = serde_json::from_str::<T>(&body).map_err(|e| {
        anyhow::anyhow!("Decode error for {url}: {e}; content-type={ct}; body={body}")
    })?;
    Ok(parsed)
}

fn get_git_origins() -> Vec<String> {
    // Prefer: git config --get-regexp remote\..*\.url
    let out = std::process::Command::new("git")
        .args(["config", "--get-regexp", "remote\\..*\\.url"])
        .output();
    if let Ok(ok) = out
        && ok.status.success()
    {
        let s = String::from_utf8_lossy(&ok.stdout);
        let mut urls = Vec::new();
        for line in s.lines() {
            if let Some((_, url)) = line.split_once(' ') {
                urls.push(url.trim().to_string());
            }
        }
        if !urls.is_empty() {
            return uniq(urls);
        }
    }
    // Fallback: git remote -v
    let out = std::process::Command::new("git")
        .args(["remote", "-v"])
        .output();
    if let Ok(ok) = out
        && ok.status.success()
    {
        let s = String::from_utf8_lossy(&ok.stdout);
        let mut urls = Vec::new();
        for line in s.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                urls.push(parts[1].to_string());
            }
        }
        if !urls.is_empty() {
            return uniq(urls);
        }
    }
    Vec::new()
}

fn uniq(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v.dedup();
    v
}

fn parse_owner_repo(url: &str) -> Option<(String, String)> {
    // Normalize common prefixes and handle multiple SSH/HTTPS variants.
    let mut s = url.trim().to_string();
    // Drop protocol scheme for ssh URLs
    if let Some(rest) = s.strip_prefix("ssh://") {
        s = rest.to_string();
    }
    // Accept any user before @github.com (e.g., git@, org-123@)
    if let Some(idx) = s.find("@github.com:") {
        let rest = &s[idx + "@github.com:".len()..];
        let rest = rest.trim_start_matches('/').trim_end_matches(".git");
        let mut parts = rest.splitn(2, '/');
        let owner = parts.next()?.to_string();
        let repo = parts.next()?.to_string();
        crate::append_error_log(format!("env: parsed SSH GitHub origin => {owner}/{repo}"));
        return Some((owner, repo));
    }
    // HTTPS or git protocol
    for prefix in [
        "https://github.com/",
        "http://github.com/",
        "git://github.com/",
        "github.com/",
    ] {
        if let Some(rest) = s.strip_prefix(prefix) {
            let rest = rest.trim_start_matches('/').trim_end_matches(".git");
            let mut parts = rest.splitn(2, '/');
            let owner = parts.next()?.to_string();
            let repo = parts.next()?.to_string();
            crate::append_error_log(format!("env: parsed HTTP GitHub origin => {owner}/{repo}"));
            return Some((owner, repo));
        }
    }
    None
}

/// List environments for the current repo(s) with a fallback to the global list.
/// Returns a de-duplicated, sorted set suitable for the TUI modal.
pub async fn list_environments(
    base_url: &str,
    headers: &HeaderMap,
) -> anyhow::Result<Vec<crate::app::EnvironmentRow>> {
    let mut map: HashMap<String, crate::app::EnvironmentRow> = HashMap::new();

    // 1) By-repo lookup for each parsed GitHub origin
    let origins = get_git_origins();
    for origin in &origins {
        if let Some((owner, repo)) = parse_owner_repo(origin) {
            let url = if base_url.contains("/backend-api") {
                format!(
                    "{}/wham/environments/by-repo/{}/{}/{}",
                    base_url, "github", owner, repo
                )
            } else {
                format!(
                    "{}/api/codex/environments/by-repo/{}/{}/{}",
                    base_url, "github", owner, repo
                )
            };
            match get_json::<Vec<CodeEnvironment>>(&url, headers).await {
                Ok(list) => {
                    info!("env_tui: by-repo {}:{} -> {} envs", owner, repo, list.len());
                    for e in list {
                        let entry =
                            map.entry(e.id.clone())
                                .or_insert_with(|| crate::app::EnvironmentRow {
                                    id: e.id.clone(),
                                    label: e.label.clone(),
                                    is_pinned: e.is_pinned.unwrap_or(false),
                                    repo_hints: Some(format!("{owner}/{repo}")),
                                });
                        // Merge: keep label if present, or use new; accumulate pinned flag
                        if entry.label.is_none() {
                            entry.label = e.label.clone();
                        }
                        entry.is_pinned = entry.is_pinned || e.is_pinned.unwrap_or(false);
                        if entry.repo_hints.is_none() {
                            entry.repo_hints = Some(format!("{owner}/{repo}"));
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        "env_tui: by-repo fetch failed for {}/{}: {}",
                        owner, repo, e
                    );
                }
            }
        }
    }

    // 2) Fallback to the full list; on error return what we have if any.
    let list_url = if base_url.contains("/backend-api") {
        format!("{base_url}/wham/environments")
    } else {
        format!("{base_url}/api/codex/environments")
    };
    match get_json::<Vec<CodeEnvironment>>(&list_url, headers).await {
        Ok(list) => {
            info!("env_tui: global list -> {} envs", list.len());
            for e in list {
                let entry = map
                    .entry(e.id.clone())
                    .or_insert_with(|| crate::app::EnvironmentRow {
                        id: e.id.clone(),
                        label: e.label.clone(),
                        is_pinned: e.is_pinned.unwrap_or(false),
                        repo_hints: None,
                    });
                if entry.label.is_none() {
                    entry.label = e.label.clone();
                }
                entry.is_pinned = entry.is_pinned || e.is_pinned.unwrap_or(false);
            }
        }
        Err(e) => {
            if map.is_empty() {
                return Err(e);
            } else {
                warn!(
                    "env_tui: global list failed; using by-repo results only: {}",
                    e
                );
            }
        }
    }

    let mut rows: Vec<crate::app::EnvironmentRow> = map.into_values().collect();
    rows.sort_by(|a, b| {
        // pinned first
        let p = b.is_pinned.cmp(&a.is_pinned);
        if p != std::cmp::Ordering::Equal {
            return p;
        }
        // then label (ci), then id
        let al = a.label.as_deref().unwrap_or("").to_lowercase();
        let bl = b.label.as_deref().unwrap_or("").to_lowercase();
        let l = al.cmp(&bl);
        if l != std::cmp::Ordering::Equal {
            return l;
        }
        a.id.cmp(&b.id)
    });
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use reqwest::StatusCode;

    use super::EnvironmentListDiagnostic;
    use super::decode_environment_list_response;

    #[test]
    fn successful_environment_response_uses_typed_result_for_diagnostics() {
        let body = r#"[{"id":"env-1","label":"Production","unknown":"ignored"}]"#;

        let (result, diagnostic) = decode_environment_list_response(
            "https://example.test/environments",
            StatusCode::OK,
            "application/json",
            body,
        );
        let environments = result.expect("typed environment response");

        assert_eq!(environments.len(), 1);
        assert_eq!(environments[0].id, "env-1");
        assert_eq!(environments[0].label.as_deref(), Some("Production"));
        assert_eq!(
            diagnostic,
            EnvironmentListDiagnostic::Pretty(
                r#"[
  {
    "id": "env-1",
    "label": "Production",
    "is_pinned": null,
    "task_count": null
  }
]"#
                .to_string()
            )
        );
    }

    #[test]
    fn malformed_success_response_keeps_raw_diagnostic_and_decode_context() {
        let body = r#"{"id":"env-1"}"#;

        let (result, diagnostic) = decode_environment_list_response(
            "https://example.test/environments",
            StatusCode::OK,
            "application/json",
            body,
        );
        let error = result.expect_err("object is not an environment list");

        assert!(
            error
                .to_string()
                .starts_with("Decode error for https://example.test/environments: "),
            "{error}"
        );
        assert!(
            error
                .to_string()
                .ends_with("; content-type=application/json; body={\"id\":\"env-1\"}"),
            "{error}"
        );
        assert_eq!(diagnostic, EnvironmentListDiagnostic::Raw(body.to_string()));
    }

    #[test]
    fn unsuccessful_environment_response_preserves_dynamic_json_diagnostic() {
        let body = r#"{"error":{"code":"bad_request"},"unknown":true}"#;

        let (result, diagnostic) = decode_environment_list_response(
            "https://example.test/environments",
            StatusCode::BAD_REQUEST,
            "application/json",
            body,
        );
        let error = result.expect_err("HTTP failure");

        assert_eq!(
            error.to_string(),
            "GET https://example.test/environments failed: 400 Bad Request; \
             content-type=application/json; \
             body={\"error\":{\"code\":\"bad_request\"},\"unknown\":true}"
        );
        assert_eq!(
            diagnostic,
            EnvironmentListDiagnostic::Pretty(
                r#"{
  "error": {
    "code": "bad_request"
  },
  "unknown": true
}"#
                .to_string()
            )
        );
    }

    #[test]
    fn unsuccessful_non_json_environment_response_keeps_raw_diagnostic() {
        let body = "upstream unavailable";

        let (result, diagnostic) = decode_environment_list_response(
            "https://example.test/environments",
            StatusCode::SERVICE_UNAVAILABLE,
            "text/plain",
            body,
        );
        let error = result.expect_err("HTTP failure");

        assert_eq!(
            error.to_string(),
            "GET https://example.test/environments failed: 503 Service Unavailable; \
             content-type=text/plain; body=upstream unavailable"
        );
        assert_eq!(diagnostic, EnvironmentListDiagnostic::Raw(body.to_string()));
    }
}
