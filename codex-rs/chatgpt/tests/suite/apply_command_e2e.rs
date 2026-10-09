use codex_chatgpt::apply_command::apply_diff_from_task;
use codex_chatgpt::get_task::GetTaskResponse;
use codex_utils_cargo_bin::find_resource;
use tempfile::TempDir;
use tokio::process::Command;

// The complete added file specified by task_turn_fixture.json, not output
// captured from apply_diff_from_task.
const EXPECTED_FIBONACCI: &str = r#"#!/usr/bin/env node

function fibonacci(n) {
  if (n < 0) {
    throw new Error("n must be non-negative");
  }
  let a = 0;
  let b = 1;
  for (let i = 0; i < n; i++) {
    const next = a + b;
    a = b;
    b = next;
  }
  return a;
}

function printUsage() {
  console.log("Usage: node scripts/fibonacci.js <n>");
}

if (require.main === module) {
  const arg = process.argv[2];
  if (arg === undefined || isNaN(Number(arg))) {
    printUsage();
    process.exit(1);
  }
  const n = Number(arg);
  console.log(fibonacci(n));
}

module.exports = fibonacci;
"#;

/// Creates a temporary git repository with initial commit
async fn create_temp_git_repo() -> anyhow::Result<TempDir> {
    let temp_dir = TempDir::new()?;
    let repo_path = temp_dir.path();
    let envs = vec![
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
    ];

    let output = Command::new("git")
        .envs(envs.clone())
        .args(["init"])
        .current_dir(repo_path)
        .output()
        .await?;

    if !output.status.success() {
        anyhow::bail!(
            "Failed to initialize git repo: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Command::new("git")
        .envs(envs.clone())
        .args(["config", "user.email", "test@example.com"])
        .current_dir(repo_path)
        .output()
        .await?;

    Command::new("git")
        .envs(envs.clone())
        .args(["config", "user.name", "Test User"])
        .current_dir(repo_path)
        .output()
        .await?;

    std::fs::write(repo_path.join("README.md"), "# Test Repo\n")?;

    Command::new("git")
        .envs(envs.clone())
        .args(["add", "README.md"])
        .current_dir(repo_path)
        .output()
        .await?;

    let output = Command::new("git")
        .envs(envs.clone())
        .args(["commit", "-m", "Initial commit"])
        .current_dir(repo_path)
        .output()
        .await?;

    if !output.status.success() {
        anyhow::bail!(
            "Failed to create initial commit: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(temp_dir)
}

async fn mock_get_task_with_fixture() -> anyhow::Result<GetTaskResponse> {
    let fixture_path = find_resource!("tests/task_turn_fixture.json")?;
    let fixture_content = tokio::fs::read_to_string(fixture_path).await?;
    let response: GetTaskResponse = serde_json::from_str(&fixture_content)?;
    Ok(response)
}

#[test]
fn test_apply_command_creates_fibonacci_file() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .expect("runtime");
    let temp_repo = runtime
        .block_on(create_temp_git_repo())
        .expect("Failed to create temp git repo");
    let repo_path = temp_repo.path();

    let task_response = runtime
        .block_on(mock_get_task_with_fixture())
        .expect("Failed to load fixture");

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
        let apply = apply_diff_from_task(task_response, Some(repo_path.to_path_buf()));
        let observer = async {
            let applied_before_yield = repo_path.join("scripts/fibonacci.js").exists();
            release_tx.send(()).expect("release patch worker");
            assert!(
                !applied_before_yield,
                "patch application must yield before changing the worktree"
            );
        };
        let (result, ()) = tokio::join!(biased; apply, observer);
        result.expect("Failed to apply diff from task");
        blocker.await.expect("blocking worker");
    });

    // Assert that fibonacci.js was created in scripts/ directory
    let fibonacci_path = repo_path.join("scripts/fibonacci.js");
    assert!(fibonacci_path.exists(), "fibonacci.js was not created");

    // Verify the file contents match expected
    let contents = std::fs::read_to_string(&fibonacci_path).expect("Failed to read fibonacci.js");
    // Git may honor a CRLF checkout policy; all logical contents must match.
    assert_eq!(contents.replace("\r\n", "\n"), EXPECTED_FIBONACCI);
}

#[tokio::test]
async fn test_apply_command_accepts_diff_carried_as_output_diff_item() {
    let temp_repo = create_temp_git_repo()
        .await
        .expect("Failed to create temp git repo");
    let repo_path = temp_repo.path();

    // The fixture's diff carried directly by the diff turn rather than inside
    // a `pr` item, a task shape `codex cloud apply` also accepts.
    let fixture_path = find_resource!("tests/task_turn_fixture.json").expect("fixture path");
    let fixture: serde_json::Value = serde_json::from_str(
        &tokio::fs::read_to_string(fixture_path)
            .await
            .expect("read fixture"),
    )
    .expect("fixture JSON");
    let output_diff = fixture["current_diff_task_turn"]["output_items"][0]["output_diff"].clone();
    let task_response: GetTaskResponse = serde_json::from_value(serde_json::json!({
        "current_diff_task_turn": {"output_items": [output_diff]}
    }))
    .expect("task response");

    apply_diff_from_task(task_response, Some(repo_path.to_path_buf()))
        .await
        .expect("apply a diff carried as an output_diff item");

    let contents = std::fs::read_to_string(repo_path.join("scripts/fibonacci.js"))
        .expect("Failed to read fibonacci.js");
    assert_eq!(contents.replace("\r\n", "\n"), EXPECTED_FIBONACCI);
}

#[tokio::test]
async fn test_apply_command_with_merge_conflicts() {
    let temp_repo = create_temp_git_repo()
        .await
        .expect("Failed to create temp git repo");
    let repo_path = temp_repo.path();

    // Create conflicting fibonacci.js file first
    let scripts_dir = repo_path.join("scripts");
    std::fs::create_dir_all(&scripts_dir).expect("Failed to create scripts directory");

    let conflicting_content = r#"#!/usr/bin/env node

// This is a different fibonacci implementation
function fib(num) {
  if (num <= 1) return num;
  return fib(num - 1) + fib(num - 2);
}

console.log("Running fibonacci...");
console.log(fib(10));
"#;

    let fibonacci_path = scripts_dir.join("fibonacci.js");
    std::fs::write(&fibonacci_path, conflicting_content).expect("Failed to write conflicting file");

    Command::new("git")
        .args(["add", "scripts/fibonacci.js"])
        .current_dir(repo_path)
        .output()
        .await
        .expect("Failed to add fibonacci.js");

    Command::new("git")
        .args(["commit", "-m", "Add conflicting fibonacci implementation"])
        .current_dir(repo_path)
        .output()
        .await
        .expect("Failed to commit conflicting file");

    let task_response = mock_get_task_with_fixture()
        .await
        .expect("Failed to load fixture");

    let error = apply_diff_from_task(task_response, Some(repo_path.to_path_buf()))
        .await
        .expect_err("Expected apply to fail due to merge conflicts");
    assert!(
        error.to_string().contains("Git apply failed")
            && error.to_string().contains("conflicts=1"),
        "expected a reported patch conflict, got: {error}"
    );

    let contents = std::fs::read_to_string(&fibonacci_path).expect("Failed to read fibonacci.js");

    // Git may write LF or CRLF according to the checkout's line-ending policy.
    assert!(
        contents.lines().any(|line| line.starts_with("<<<<<<< "))
            && contents.lines().any(|line| line == "=======")
            && contents.lines().any(|line| line.starts_with(">>>>>>> "))
            && contents.contains("function fib(num)")
            && contents.contains("function fibonacci(n)"),
        "fibonacci.js should contain merge conflict markers, got: {contents}",
    );
}
