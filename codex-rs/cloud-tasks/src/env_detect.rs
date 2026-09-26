use anyhow::Context;
use codex_http_client::RouteAwareClientPool;
use http::HeaderMap;
use http::header::CONTENT_TYPE;
use std::collections::HashMap;
use std::time::Duration;
use tracing::info;
use tracing::warn;

use crate::urls::CloudBaseUrl;

/// The TUI runs one environment fetch at a time, so every request must end: a stalled one
/// would otherwise keep every later environment refresh from starting.
const ENVIRONMENT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, serde::Deserialize)]
struct CodeEnvironment {
    id: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    is_pinned: Option<bool>,
    #[serde(default)]
    task_count: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct AutodetectSelection {
    pub id: String,
    pub label: Option<String>,
}

/// Environment lists fetched once for both the picker rows and autodetection.
struct EnvironmentLists {
    /// Environments linked to each parsed GitHub origin, keyed by `owner/repo`.
    by_repo: Vec<(String, Vec<CodeEnvironment>)>,
    /// The workspace-wide list.
    global: anyhow::Result<Vec<CodeEnvironment>>,
}

/// Loads the environment rows and picks the likely environment for this repository from one
/// set of requests: repository-linked environments win, then the workspace-wide list.
pub async fn load_environments_and_autodetect(
    http: &RouteAwareClientPool,
    base_url: &CloudBaseUrl,
    headers: &HeaderMap,
) -> (
    anyhow::Result<Vec<crate::app::EnvironmentRow>>,
    anyhow::Result<AutodetectSelection>,
) {
    let origins = match get_git_origins().await {
        Ok(origins) => origins,
        Err(error) => {
            let autodetect_error = anyhow::anyhow!("{error:#}");
            return (Err(error), Err(autodetect_error));
        }
    };
    let lists = fetch_environment_lists(
        http,
        base_url,
        headers,
        &origins,
        ENVIRONMENT_REQUEST_TIMEOUT,
    )
    .await;
    let autodetected = autodetect_from_lists(&lists);
    (environment_rows(lists), autodetected)
}

/// List environments for the current repo(s) with a fallback to the global list.
/// Returns a de-duplicated, sorted set suitable for the TUI modal.
pub async fn list_environments(
    http: &RouteAwareClientPool,
    base_url: &CloudBaseUrl,
    headers: &HeaderMap,
) -> anyhow::Result<Vec<crate::app::EnvironmentRow>> {
    let origins = get_git_origins().await?;
    environment_rows(
        fetch_environment_lists(
            http,
            base_url,
            headers,
            &origins,
            ENVIRONMENT_REQUEST_TIMEOUT,
        )
        .await,
    )
}

async fn fetch_environment_lists(
    http: &RouteAwareClientPool,
    base_url: &CloudBaseUrl,
    headers: &HeaderMap,
    origins: &[String],
    request_timeout: Duration,
) -> EnvironmentLists {
    let mut by_repo = Vec::new();
    for origin in origins {
        let Some((owner, repo)) = parse_owner_repo(origin) else {
            continue;
        };
        let url = environments_url(base_url, Some((&owner, &repo)));
        crate::append_error_log(format!("env: GET {url}"));
        match get_json_with_client::<Vec<CodeEnvironment>>(http, &url, headers, request_timeout)
            .await
        {
            Ok(list) => {
                info!("env_tui: by-repo {}:{} -> {} envs", owner, repo, list.len());
                crate::append_error_log(format!(
                    "env: by-repo returned {} env(s) for {owner}/{repo}",
                    list.len(),
                ));
                by_repo.push((format!("{owner}/{repo}"), list));
            }
            Err(e) => {
                warn!(
                    "env_tui: by-repo fetch failed for {}/{}: {}",
                    owner, repo, e
                );
                crate::append_error_log(format!(
                    "env: by-repo fetch failed for {owner}/{repo}: {e}"
                ));
            }
        }
    }

    let list_url = environments_url(base_url, /*repo*/ None);
    crate::append_error_log(format!("env: GET {list_url}"));
    let global =
        get_json_with_client::<Vec<CodeEnvironment>>(http, &list_url, headers, request_timeout)
            .await;
    match &global {
        Ok(list) => {
            info!("env_tui: global list -> {} envs", list.len());
            crate::append_error_log(format!("env: global list returned {} env(s)", list.len()));
        }
        Err(e) => crate::append_error_log(format!("env: global list fetch failed: {e}")),
    }
    EnvironmentLists { by_repo, global }
}

fn environments_url(base_url: &CloudBaseUrl, repo: Option<(&str, &str)>) -> String {
    let root = if base_url.as_str().contains("/backend-api") {
        format!("{base_url}/wham/environments")
    } else {
        format!("{base_url}/api/codex/environments")
    };
    match repo {
        Some((owner, repo)) => format!("{root}/by-repo/github/{owner}/{repo}"),
        None => root,
    }
}

fn autodetect_from_lists(lists: &EnvironmentLists) -> anyhow::Result<AutodetectSelection> {
    let by_repo_envs = lists
        .by_repo
        .iter()
        .flat_map(|(_, envs)| envs.iter().cloned())
        .collect::<Vec<_>>();
    let env = match pick_environment_row(&by_repo_envs) {
        Some(env) => env,
        None => {
            let all_envs = match &lists.global {
                Ok(all_envs) => all_envs,
                // The picker reports the structured error; this result only selects a filter.
                Err(error) => anyhow::bail!("{error:#}"),
            };
            pick_environment_row(all_envs)
                .ok_or_else(|| anyhow::anyhow!("no environments available"))?
        }
    };
    Ok(AutodetectSelection {
        id: env.id,
        label: env.label,
    })
}

fn pick_environment_row(envs: &[CodeEnvironment]) -> Option<CodeEnvironment> {
    if envs.is_empty() {
        return None;
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

fn environment_rows(lists: EnvironmentLists) -> anyhow::Result<Vec<crate::app::EnvironmentRow>> {
    let mut map: HashMap<String, crate::app::EnvironmentRow> = HashMap::new();

    // 1) Environments linked to each parsed GitHub origin
    for (repo_hint, list) in lists.by_repo {
        for e in list {
            let entry = map
                .entry(e.id.clone())
                .or_insert_with(|| crate::app::EnvironmentRow {
                    id: e.id.clone(),
                    label: e.label.clone(),
                    is_pinned: e.is_pinned.unwrap_or(false),
                    repo_hints: Some(repo_hint.clone()),
                });
            // Merge: keep label if present, or use new; accumulate pinned flag
            if entry.label.is_none() {
                entry.label = e.label.clone();
            }
            entry.is_pinned = entry.is_pinned || e.is_pinned.unwrap_or(false);
            if entry.repo_hints.is_none() {
                entry.repo_hints = Some(repo_hint.clone());
            }
        }
    }

    // 2) Fallback to the full list; on error return what we have if any.
    match lists.global {
        Ok(list) => {
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

async fn get_json_with_client<T: serde::de::DeserializeOwned>(
    http: &RouteAwareClientPool,
    url: &str,
    headers: &HeaderMap,
    request_timeout: Duration,
) -> anyhow::Result<T> {
    let res = match http
        .get(url)
        .headers(headers.clone())
        .timeout(request_timeout)
        .send()
        .await
    {
        Ok(res) => res,
        Err(error) if error.is_timeout() => {
            return Err(anyhow::Error::new(error)
                .context(format!("GET {url} timed out after {request_timeout:?}")));
        }
        Err(error) => return Err(error.into()),
    };
    let status = res.status();
    let ct = res
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = res
        .text()
        .await
        .with_context(|| format!("Failed to read response body from {url}"))?;
    crate::append_error_log(format!("env: status={status} content-type={ct}"));
    if !status.is_success() {
        anyhow::bail!("GET {url} failed: {status}; content-type={ct}; body={body}");
    }
    let parsed = serde_json::from_str::<T>(&body).map_err(|e| {
        anyhow::anyhow!("Decode error for {url}: {e}; content-type={ct}; body={body}")
    })?;
    Ok(parsed)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::items_after_test_module)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn environment_requests_preserve_response_body_transport_errors() {
        use tokio::io::AsyncReadExt;
        use tokio::io::AsyncWriteExt;

        for combined in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind response server");
            let address = listener.local_addr().expect("server address");
            let server = tokio::spawn(async move {
                loop {
                    let (mut stream, _) = listener.accept().await.expect("accept request");
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") {
                        let byte = stream.read_u8().await.expect("read request headers");
                        request.push(byte);
                    }
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 16\r\nConnection: close\r\n\r\n[]")
                        .await
                        .expect("send deliberately truncated response");
                    stream.shutdown().await.expect("close response");
                }
            });
            let http = crate::environment_http_clients(&codex_http_client::HttpClientFactory::new(
                codex_http_client::OutboundProxyPolicy::ReqwestDefault,
            ));
            let base_url = CloudBaseUrl::new(&format!("http://{address}/backend-api"));
            let result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                if combined {
                    let (rows, autodetected) =
                        load_environments_and_autodetect(&http, &base_url, &HeaderMap::new()).await;
                    assert!(autodetected.is_err(), "no environment can be selected");
                    rows.map(|_| ())
                } else {
                    list_environments(&http, &base_url, &HeaderMap::new())
                        .await
                        .map(|_| ())
                }
            })
            .await;
            server.abort();
            let _ = server.await;
            let error = result
                .expect("environment request must finish")
                .expect_err("truncated HTTP body must fail");
            assert!(
                error
                    .to_string()
                    .starts_with("Failed to read response body from ")
            );
            assert!(
                error.chain().count() > 1,
                "preserve the transport cause: {error:#}"
            );
            assert!(!format!("{error:#}").contains("Decode error"));
        }
    }

    #[tokio::test]
    async fn stalled_environment_requests_time_out_instead_of_holding_the_fetch_slot() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stalled server");
        let address = listener.local_addr().expect("server address");
        // Accept every request and never answer it.
        let server = tokio::spawn(async move {
            let mut connections = Vec::new();
            loop {
                let (stream, _) = listener.accept().await.expect("accept request");
                connections.push(stream);
            }
        });
        let http = crate::environment_http_clients(&codex_http_client::HttpClientFactory::new(
            codex_http_client::OutboundProxyPolicy::ReqwestDefault,
        ));
        let base_url = CloudBaseUrl::new(&format!("http://{address}/backend-api"));
        let origins = vec!["https://github.com/owner/repo.git".to_string()];

        let lists = tokio::time::timeout(
            Duration::from_secs(10),
            fetch_environment_lists(
                &http,
                &base_url,
                &HeaderMap::new(),
                &origins,
                Duration::from_millis(100),
            ),
        )
        .await
        .expect("stalled environment requests must end");
        server.abort();
        let _ = server.await;

        assert!(
            lists.by_repo.is_empty(),
            "the stalled repository request is skipped"
        );
        let error = environment_rows(lists).expect_err("no environment list arrived");
        assert!(
            error
                .downcast_ref::<codex_http_client::RouteAwareRequestError>()
                .is_some_and(codex_http_client::RouteAwareRequestError::is_timeout),
            "{error:#}"
        );
        assert!(
            error.to_string().ends_with("timed out after 100ms"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn startup_loads_rows_and_autodetects_from_one_environment_list_request() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/backend-api/wham/environments"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!([{"id": "env-1", "label": "Local"}])),
            )
            // Autodetection reuses the picker's request instead of listing again.
            .expect(1)
            .mount(&server)
            .await;
        let http = crate::environment_http_clients(&codex_http_client::HttpClientFactory::new(
            codex_http_client::OutboundProxyPolicy::ReqwestDefault,
        ));

        let base_url = CloudBaseUrl::new(&format!("{}/backend-api", server.uri()));
        let (environments, selected) =
            load_environments_and_autodetect(&http, &base_url, &HeaderMap::new()).await;
        let environments = environments.expect("environment response should decode");
        let selected = selected.expect("environment should be selected after Git discovery");

        assert_eq!(environments.len(), 1);
        assert_eq!(environments[0].id, "env-1");
        assert_eq!(environments[0].label.as_deref(), Some("Local"));
        assert_eq!(selected.id, "env-1");
        assert_eq!(selected.label.as_deref(), Some("Local"));
    }

    #[test]
    fn autodetection_prefers_repository_environments_over_the_workspace_list() {
        let env = |id: &str, pinned: bool, task_count: i64| CodeEnvironment {
            id: id.to_string(),
            label: Some(format!("{id} label")),
            is_pinned: Some(pinned),
            task_count: Some(task_count),
        };
        let lists = EnvironmentLists {
            by_repo: vec![(
                "owner/repo".to_string(),
                vec![env("repo-a", false, 1), env("repo-b", false, 9)],
            )],
            global: Ok(vec![env("pinned-global", true, 50)]),
        };
        let selected = autodetect_from_lists(&lists).expect("repository environment");
        assert_eq!(selected.id, "repo-b");

        let rows = environment_rows(lists).expect("rows");
        assert_eq!(
            rows.iter()
                .map(|row| (row.id.as_str(), row.repo_hints.as_deref()))
                .collect::<Vec<_>>(),
            vec![
                ("pinned-global", None),
                ("repo-a", Some("owner/repo")),
                ("repo-b", Some("owner/repo")),
            ]
        );

        let workspace_only = EnvironmentLists {
            by_repo: Vec::new(),
            global: Ok(vec![
                env("global-a", false, 1),
                env("pinned-global", true, 0),
            ]),
        };
        assert_eq!(
            autodetect_from_lists(&workspace_only)
                .expect("workspace environment")
                .id,
            "pinned-global"
        );
    }

    #[test]
    fn git_origin_discovery_yields_to_the_async_scheduler() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .expect("runtime");
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocker = runtime.spawn_blocking(move || {
            started_tx.send(()).expect("signal worker start");
            release_rx
                .recv_timeout(std::time::Duration::from_secs(30))
                .expect("scheduler must release worker");
        });
        started_rx.recv().expect("worker started");

        runtime.block_on(async {
            let finished = std::cell::Cell::new(false);
            let discovery = async {
                let origins = get_git_origins().await.expect("Git discovery");
                finished.set(true);
                origins
            };
            let observer = async {
                let completed_before_yield = finished.get();
                release_tx.send(()).expect("release Git worker");
                assert!(
                    !completed_before_yield,
                    "Git discovery must yield before running synchronous commands"
                );
            };
            let (origins, ()) = tokio::join!(biased; discovery, observer);
            blocker.await.expect("blocking worker");
            assert!(origins.windows(2).all(|pair| pair[0] < pair[1]));
        });
    }

    #[test]
    fn environment_pool_retains_the_effective_proxy_policy() {
        let http = crate::environment_http_clients(&codex_http_client::HttpClientFactory::new(
            codex_http_client::OutboundProxyPolicy::RespectSystemProxy,
        ));

        assert_eq!(
            http.outbound_proxy_policy(),
            codex_http_client::OutboundProxyPolicy::RespectSystemProxy
        );
    }
}

async fn get_git_origins() -> anyhow::Result<Vec<String>> {
    Ok(tokio::task::spawn_blocking(read_git_origins).await?)
}

fn read_git_origins() -> Vec<String> {
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
