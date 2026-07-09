use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

pub(crate) fn emit() {
    println!("cargo:rerun-if-env-changed=CODEX_BUILD_COMMIT");
    println!("cargo:rerun-if-env-changed=CODEX_BUILD_DIRTY");
    println!("cargo:rerun-if-env-changed=CODEX_BUILD_PROFILE");
    println!("cargo:rerun-if-env-changed=CODEX_BUILD_TIMESTAMP");
    println!("cargo:rerun-if-env-changed=PROFILE");

    let Some(manifest_dir) = std::env::var_os("CARGO_MANIFEST_DIR") else {
        eprintln!("CARGO_MANIFEST_DIR should be set for build scripts");
        std::process::exit(1);
    };
    let manifest_dir = PathBuf::from(manifest_dir);
    let Some(workspace_root) = workspace_root(&manifest_dir) else {
        eprintln!("workspace root should be resolvable from CARGO_MANIFEST_DIR");
        std::process::exit(1);
    };
    println!(
        "cargo:rerun-if-changed={}",
        workspace_root.join("build_info.rs").display()
    );
    if let Some(git_dir) = git_dir(&workspace_root) {
        println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
        println!("cargo:rerun-if-changed={}", git_dir.join("index").display());
        println!(
            "cargo:rerun-if-changed={}",
            common_git_dir(&git_dir)
                .unwrap_or_else(|| git_dir.clone())
                .join("packed-refs")
                .display()
        );
        if let Some(ref_name) = git_head_ref(&git_dir) {
            for ref_path in git_ref_paths_from_dir(&git_dir, &ref_name) {
                println!("cargo:rerun-if-changed={}", ref_path.display());
            }
        }
    }

    let commit = std::env::var("CODEX_BUILD_COMMIT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| git_head_commit(&workspace_root))
        .unwrap_or_else(|| "unknown".to_string());
    let dirty = std::env::var("CODEX_BUILD_DIRTY")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| git_dirty(&workspace_root))
        .unwrap_or_else(|| "unknown".to_string());
    let profile = std::env::var("CODEX_BUILD_PROFILE")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| std::env::var("PROFILE").ok())
        .unwrap_or_else(|| "unknown".to_string());
    let timestamp = std::env::var("CODEX_BUILD_TIMESTAMP")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=CODEX_BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=CODEX_BUILD_DIRTY={dirty}");
    println!("cargo:rustc-env=CODEX_BUILD_PROFILE={profile}");
    println!("cargo:rustc-env=CODEX_BUILD_TIMESTAMP={timestamp}");
}

fn workspace_root(manifest_dir: &Path) -> Option<PathBuf> {
    manifest_dir.parent().map(Path::to_path_buf)
}

fn git_head_commit(workspace_root: &Path) -> Option<String> {
    if let Some(commit) = git_output(workspace_root, &["rev-parse", "--short=12", "HEAD"]) {
        return Some(commit);
    }

    let head_path = git_path(workspace_root, "HEAD")?;
    let head = read_trimmed(&head_path)?;
    if let Some(ref_name) = head.strip_prefix("ref: ") {
        let ref_name = ref_name.trim();
        let git_dir = git_dir(workspace_root)?;
        for ref_path in git_ref_paths_from_dir(&git_dir, ref_name) {
            if let Some(value) = read_trimmed(&ref_path) {
                return Some(short_hash(&value));
            }
        }

        return git_packed_ref(workspace_root, ref_name).map(|value| short_hash(&value));
    }

    Some(short_hash(&head))
}

fn git_dirty(workspace_root: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(workspace_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    Some(if output.stdout.is_empty() {
        "false".to_string()
    } else {
        "true".to_string()
    })
}

fn git_output(workspace_root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(workspace_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn git_packed_ref(workspace_root: &Path, ref_name: &str) -> Option<String> {
    let git_dir = git_dir(workspace_root)?;
    [Some(git_dir.clone()), common_git_dir(&git_dir)]
        .into_iter()
        .flatten()
        .find_map(|dir| {
            let packed_refs = fs::read_to_string(dir.join("packed-refs")).ok()?;
            packed_refs.lines().find_map(|line| {
                let (hash, name) = line.split_once(' ')?;
                (name == ref_name).then(|| hash.to_string())
            })
        })
}

fn git_path(workspace_root: &Path, relative: &str) -> Option<PathBuf> {
    Some(git_dir(workspace_root)?.join(relative))
}

fn git_ref_paths_from_dir(git_dir: &Path, relative: &str) -> Vec<PathBuf> {
    let mut paths = vec![git_dir.join(relative)];
    if let Some(common_dir) = common_git_dir(git_dir) {
        paths.push(common_dir.join(relative));
    }
    paths
}

fn git_head_ref(git_dir: &Path) -> Option<String> {
    read_trimmed(&git_dir.join("HEAD"))?
        .strip_prefix("ref: ")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn git_dir(workspace_root: &Path) -> Option<PathBuf> {
    for root in workspace_root.ancestors() {
        let dot_git = root.join(".git");
        if dot_git.is_dir() {
            return Some(dot_git);
        }
        if !dot_git.is_file() {
            continue;
        }

        let gitdir = read_trimmed(&dot_git)?;
        let gitdir = gitdir.strip_prefix("gitdir:")?.trim();
        let path = PathBuf::from(gitdir);
        return Some(if path.is_absolute() {
            path
        } else {
            root.join(path)
        });
    }

    None
}

fn common_git_dir(git_dir: &Path) -> Option<PathBuf> {
    let common_dir = read_trimmed(&git_dir.join("commondir"))?;
    let path = PathBuf::from(common_dir);
    Some(if path.is_absolute() {
        path
    } else {
        git_dir.join(path)
    })
}

fn read_trimmed(path: &Path) -> Option<String> {
    let value = fs::read_to_string(path).ok()?;
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn short_hash(value: &str) -> String {
    value.chars().take(12).collect()
}
