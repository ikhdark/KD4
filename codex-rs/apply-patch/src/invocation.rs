use std::collections::HashMap;
use std::sync::LazyLock;

use codex_exec_server::ExecutorFileSystem;
use tree_sitter::Parser;
use tree_sitter::Query;
use tree_sitter::QueryCursor;
use tree_sitter::StreamingIterator;
use tree_sitter_bash::LANGUAGE as BASH;

use crate::ApplyPatchAction;
use crate::ApplyPatchArgs;
use crate::ApplyPatchError;
use crate::ApplyPatchFileChange;
use crate::ApplyPatchFileUpdate;
use crate::IoError;
use crate::MaybeApplyPatchVerified;
use crate::parser::Hunk;
use crate::parser::ParseError;
use crate::parser::parse_patch;
use codex_utils_path_uri::PathConvention;
use codex_utils_path_uri::PathUri;
use std::str::Utf8Error;
use tree_sitter::LanguageError;

const APPLY_PATCH_COMMANDS: [&str; 2] = ["apply_patch", "applypatch"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApplyPatchShell {
    Unix,
    PowerShell,
    Cmd,
}

#[derive(Debug, PartialEq)]
enum MaybeApplyPatch {
    Body(ApplyPatchArgs),
    ShellParseError(ExtractHeredocError),
    PatchParseError(ParseError),
    NotApplyPatch,
}

#[derive(Debug, PartialEq)]
pub enum ExtractHeredocError {
    CommandDidNotStartWithApplyPatch,
    FailedToLoadBashGrammar(LanguageError),
    HeredocNotUtf8(Utf8Error),
    FailedToParsePatchIntoAst,
}

fn classify_shell_name(shell: &str, convention: PathConvention) -> Option<String> {
    let basename = convention.path_segments(shell).next_back()?;
    let stem = basename
        .rsplit_once('.')
        .and_then(|(stem, _extension)| (!stem.is_empty()).then_some(stem))
        .unwrap_or(basename);
    Some(stem.to_ascii_lowercase())
}

fn classify_shell(shell: &str, flag: &str, convention: PathConvention) -> Option<ApplyPatchShell> {
    classify_shell_name(shell, convention).and_then(|name| match name.as_str() {
        "bash" | "zsh" | "sh" if matches!(flag, "-lc" | "-c") => Some(ApplyPatchShell::Unix),
        "pwsh" | "powershell" if flag.eq_ignore_ascii_case("-command") => {
            Some(ApplyPatchShell::PowerShell)
        }
        "cmd" if flag.eq_ignore_ascii_case("/c") => Some(ApplyPatchShell::Cmd),
        _ => None,
    })
}

fn can_skip_flag(shell: &str, flag: &str, convention: PathConvention) -> bool {
    classify_shell_name(shell, convention).is_some_and(|name| {
        matches!(name.as_str(), "pwsh" | "powershell") && flag.eq_ignore_ascii_case("-noprofile")
    })
}

fn parse_shell_script<'a>(argv: &'a [String], cwd: &PathUri) -> Option<(ApplyPatchShell, &'a str)> {
    let convention = cwd.infer_path_convention()?;
    match argv {
        [shell, flag, script] => classify_shell(shell, flag, convention).map(|shell_type| {
            let script = script.as_str();
            (shell_type, script)
        }),
        [shell, skip_flag, flag, script] => {
            if !can_skip_flag(shell, skip_flag, convention) {
                return None;
            }
            classify_shell(shell, flag, convention).map(|shell_type| {
                let script = script.as_str();
                (shell_type, script)
            })
        }
        _ => None,
    }
}

fn extract_apply_patch_from_shell(
    shell: ApplyPatchShell,
    script: &str,
) -> std::result::Result<(String, Option<String>), ExtractHeredocError> {
    match shell {
        // Models occasionally emit the canonical POSIX heredoc even when the selected host
        // shell is PowerShell or cmd.exe. The Bash parser below recognizes only the complete,
        // single-statement apply_patch forms, so accepting that exact shape is safe and avoids
        // sending syntax that the host shell cannot execute.
        ApplyPatchShell::Unix | ApplyPatchShell::PowerShell | ApplyPatchShell::Cmd => {
            extract_apply_patch_from_bash(script)
        }
    }
}

/// `cwd` supplies the path convention used to interpret the shell executable in `argv`.
fn maybe_parse_apply_patch(argv: &[String], cwd: &PathUri) -> MaybeApplyPatch {
    match argv {
        // Direct invocation: apply_patch <patch>
        [cmd, body] if APPLY_PATCH_COMMANDS.contains(&cmd.as_str()) => match parse_patch(body) {
            Ok(source) => MaybeApplyPatch::Body(source),
            Err(e) => MaybeApplyPatch::PatchParseError(e),
        },
        // Shell heredoc form: (optional `cd <path> &&`) apply_patch <<'EOF' ...
        _ => match parse_shell_script(argv, cwd) {
            Some((shell, script)) => match extract_apply_patch_from_shell(shell, script) {
                Ok((body, workdir)) => match parse_patch(&body) {
                    Ok(mut source) => {
                        source.workdir = workdir;
                        MaybeApplyPatch::Body(source)
                    }
                    Err(e) => MaybeApplyPatch::PatchParseError(e),
                },
                Err(ExtractHeredocError::CommandDidNotStartWithApplyPatch) => {
                    MaybeApplyPatch::NotApplyPatch
                }
                Err(e) => MaybeApplyPatch::ShellParseError(e),
            },
            None => MaybeApplyPatch::NotApplyPatch,
        },
    }
}

/// Resolves a parsed patch command's working directory without reading the filesystem.
/// Callers can acquire that workspace's mutation guard before filesystem verification.
/// Non-patch commands and invalid paths return `None`; verification reports their errors.
pub fn apply_patch_command_cwd(argv: &[String], cwd: &PathUri) -> Option<PathUri> {
    apply_patch_command_scope(argv, cwd).map(|(patch_cwd, _hunks)| patch_cwd)
}

/// Like [`apply_patch_command_cwd`], and also returns the parsed hunks, whose
/// paths are relative to that directory, so callers can claim the patch's own
/// endpoints rather than the whole workspace before verification.
pub fn apply_patch_command_scope(argv: &[String], cwd: &PathUri) -> Option<(PathUri, Vec<Hunk>)> {
    let MaybeApplyPatch::Body(args) = maybe_parse_apply_patch(argv, cwd) else {
        return None;
    };
    let patch_cwd = match args.workdir {
        Some(dir) => cwd.join(&dir).ok()?,
        None => cwd.clone(),
    };
    Some((patch_cwd, args.hunks))
}

/// `cwd` must identify an absolute environment-native path so relative patch paths can be
/// resolved without projecting them onto the app-server or exec-server host.
pub async fn maybe_parse_apply_patch_verified(
    argv: &[String],
    cwd: &PathUri,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&codex_exec_server::FileSystemSandboxContext>,
) -> MaybeApplyPatchVerified {
    maybe_parse_apply_patch_verified_inner(argv, cwd, fs, sandbox, None).await
}

/// Parses and verifies an intercepted patch against the environment already selected by the
/// containing shell command. An explicit patch environment header may confirm that selection, but
/// it must not redirect the patch to a different environment.
pub async fn maybe_parse_apply_patch_verified_for_environment(
    argv: &[String],
    cwd: &PathUri,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&codex_exec_server::FileSystemSandboxContext>,
    selected_environment_id: &str,
) -> MaybeApplyPatchVerified {
    maybe_parse_apply_patch_verified_inner(argv, cwd, fs, sandbox, Some(selected_environment_id))
        .await
}

async fn maybe_parse_apply_patch_verified_inner(
    argv: &[String],
    cwd: &PathUri,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&codex_exec_server::FileSystemSandboxContext>,
    selected_environment_id: Option<&str>,
) -> MaybeApplyPatchVerified {
    // Detect a raw patch body passed directly as the command or as the body of a shell
    // script. In these cases, report an explicit error rather than applying the patch.
    if let [body] = argv
        && body.contains(crate::parser::BEGIN_PATCH_MARKER)
        && parse_patch(body).is_ok()
    {
        return MaybeApplyPatchVerified::CorrectnessError(ApplyPatchError::ImplicitInvocation);
    }
    if let Some((_, script)) = parse_shell_script(argv, cwd) {
        if script.contains(crate::parser::BEGIN_PATCH_MARKER) {
            if parse_patch(script).is_ok() {
                return MaybeApplyPatchVerified::CorrectnessError(
                    ApplyPatchError::ImplicitInvocation,
                );
            }
        } else if !APPLY_PATCH_COMMANDS
            .iter()
            .any(|name| script.contains(name))
        {
            return MaybeApplyPatchVerified::NotApplyPatch;
        }
    }

    match maybe_parse_apply_patch(argv, cwd) {
        MaybeApplyPatch::Body(args) => {
            if let Some(selected_environment_id) = selected_environment_id
                && let Some(patch_environment_id) = args.environment_id.as_deref()
                && patch_environment_id != selected_environment_id
            {
                return MaybeApplyPatchVerified::CorrectnessError(
                    ApplyPatchError::EnvironmentIdMismatch {
                        patch_environment_id: patch_environment_id.to_string(),
                        selected_environment_id: selected_environment_id.to_string(),
                    },
                );
            }
            verify_apply_patch_args(args, cwd, fs, sandbox).await
        }
        MaybeApplyPatch::ShellParseError(e) => MaybeApplyPatchVerified::ShellParseError(e),
        MaybeApplyPatch::PatchParseError(e) => MaybeApplyPatchVerified::CorrectnessError(e.into()),
        MaybeApplyPatch::NotApplyPatch => MaybeApplyPatchVerified::NotApplyPatch,
    }
}

pub async fn verify_apply_patch_args(
    args: ApplyPatchArgs,
    cwd: &PathUri,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&codex_exec_server::FileSystemSandboxContext>,
) -> MaybeApplyPatchVerified {
    match try_verify_apply_patch_args(args, cwd, fs, sandbox).await {
        Ok(action) => MaybeApplyPatchVerified::Body(action),
        Err(err) => MaybeApplyPatchVerified::CorrectnessError(err),
    }
}

async fn try_verify_apply_patch_args(
    args: ApplyPatchArgs,
    cwd: &PathUri,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&codex_exec_server::FileSystemSandboxContext>,
) -> Result<ApplyPatchAction, ApplyPatchError> {
    let ApplyPatchArgs {
        patch,
        hunks,
        workdir,
        ..
    } = args;
    let effective_cwd = workdir
        .as_ref()
        .map(|dir| cwd.join(dir))
        .transpose()?
        .unwrap_or_else(|| cwd.clone());
    if workdir.is_some() && !fs.get_metadata(&effective_cwd, sandbox).await?.is_directory {
        return Err(ParseError::InvalidPatchError(format!(
            "cd target is not a directory: {}",
            effective_cwd.inferred_native_path_string()
        ))
        .into());
    }

    let previous_updates = validate_mutation_endpoints(&hunks, &effective_cwd, fs, sandbox).await?;
    // Preflight has already read every delete and update source and derived
    // each update, so the proposed changes reuse that snapshot.
    let prepared =
        crate::preflight_hunks(&hunks, &previous_updates, &effective_cwd, fs, sandbox).await?;
    let destination_preimages =
        crate::capture_destination_preimages(&hunks, &effective_cwd, fs, sandbox).await?;

    let mut changes = HashMap::new();
    let mut updates = HashMap::new();
    // Repeated updates of one file propose one change under the first hunk's
    // path, from the source it read to the last hunk's result.
    let mut first_updates = HashMap::new();
    for (index, (hunk, prepared)) in hunks.into_iter().zip(prepared).enumerate() {
        let mut path = hunk.resolve_path(&effective_cwd)?;
        let change = match (hunk, prepared) {
            (Hunk::AddFile { contents, .. }, _) => ApplyPatchFileChange::Add { content: contents },
            (Hunk::DeleteFile { .. }, Some(prepared)) => ApplyPatchFileChange::Delete {
                content: prepared.original_contents,
            },
            (Hunk::UpdateFile { move_path, .. }, Some(mut prepared)) => {
                let first = crate::first_update(&previous_updates, index);
                if first != index {
                    if let Some((first_path, source)) = first_updates.get(&first) {
                        path = PathUri::clone(first_path);
                        prepared.original_contents = String::clone(source);
                    }
                } else if previous_updates.contains(&Some(index)) {
                    first_updates.insert(index, (path.clone(), prepared.original_contents.clone()));
                }
                // Preflight validated every intermediate result. Only the last
                // result is observable, so do not render discarded previews.
                updates.insert(
                    path,
                    (
                        prepared,
                        move_path
                            .map(|path| effective_cwd.join(&path.to_string_lossy()))
                            .transpose()?,
                    ),
                );
                continue;
            }
            (Hunk::DeleteFile { .. } | Hunk::UpdateFile { .. }, None) => {
                return Err(ApplyPatchError::ComputeReplacements(format!(
                    "patch preflight did not prepare {}",
                    path.inferred_native_path_string()
                )));
            }
        };
        changes.insert(path, change);
    }
    for (path, (prepared, move_path)) in updates {
        let ApplyPatchFileUpdate {
            unified_diff,
            content: new_content,
            ..
        } = prepared.into_file_update(/*context*/ 1).await?;
        changes.insert(
            path,
            ApplyPatchFileChange::Update {
                unified_diff,
                move_path,
                new_content,
            },
        );
    }
    Ok(ApplyPatchAction {
        changes,
        destination_preimages,
        patch,
        cwd: effective_cwd,
    })
}

/// Check the complete endpoint set before either verified or standalone writes.
/// Repeated `Update File` hunks without a move edit one file in sequence: for
/// each hunk, the result names the earlier hunk whose result it edits.
pub(crate) async fn validate_mutation_endpoints(
    hunks: &[Hunk],
    cwd: &PathUri,
    fs: &dyn ExecutorFileSystem,
    sandbox: Option<&codex_exec_server::FileSystemSandboxContext>,
) -> Result<Vec<Option<usize>>, ApplyPatchError> {
    let mut mutation_endpoints = HashMap::new();
    let mut previous_updates = vec![None; hunks.len()];
    for (hunk_index, hunk) in hunks.iter().enumerate() {
        let source_path = hunk.resolve_path(cwd)?;
        let move_path = match hunk {
            Hunk::UpdateFile {
                move_path: Some(move_path),
                ..
            } => Some(cwd.join(&move_path.to_string_lossy())?),
            _ => None,
        };
        let in_place = matches!(
            hunk,
            Hunk::UpdateFile {
                move_path: None,
                ..
            }
        );

        for path in std::iter::once(source_path).chain(move_path) {
            let identity = mutation_endpoint_identity(fs, &path, sandbox).await?;
            if let Some((previous_path, previous_hunk, previous_in_place)) =
                mutation_endpoints.insert(identity, (path.clone(), hunk_index + 1, in_place))
            {
                if in_place && previous_in_place {
                    previous_updates[hunk_index] = Some(previous_hunk - 1);
                    continue;
                }
                let same_move = if previous_hunk == hunk_index + 1 {
                    "move source and destination identify the same path; "
                } else {
                    ""
                };
                return Err(ParseError::InvalidPatchError(format!(
                    "{same_move}path '{}' (hunk {}) is mutated more than once in the same patch; it identifies the same endpoint as '{}' (hunk {previous_hunk}). Combine edits into one Update File hunk, including edits to a move destination.",
                    path.inferred_native_path_string(),
                    hunk_index + 1,
                    previous_path.inferred_native_path_string(),
                ))
                .into());
            }
        }
    }

    Ok(previous_updates)
}

/// Resolve existing endpoints through links and resolve the closest existing
/// ancestor of prospective endpoints. This prevents differently-spelled paths
/// from bypassing the one-mutation-per-endpoint invariant.
pub(crate) async fn mutation_endpoint_identity(
    fs: &dyn ExecutorFileSystem,
    path: &PathUri,
    sandbox: Option<&codex_exec_server::FileSystemSandboxContext>,
) -> Result<String, ApplyPatchError> {
    let mut candidate = path.clone();
    let mut missing_segments = Vec::new();
    let mut resolved = loop {
        match fs.canonicalize(&candidate, sandbox).await {
            Ok(resolved) => break resolved,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                // Canonicalization follows links. A present unresolved link is
                // not a prospective lexical endpoint: publication follows it.
                match fs.get_metadata(&candidate, sandbox).await {
                    Ok(metadata) if metadata.is_symlink => {
                        return Err(ApplyPatchError::IoError(IoError {
                            context: format!(
                                "Refusing unresolved symlink mutation endpoint {}",
                                path.inferred_native_path_string()
                            ),
                            source,
                        }));
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(source) => {
                        return Err(ApplyPatchError::IoError(IoError {
                            context: format!(
                                "Failed to inspect mutation endpoint {}",
                                path.inferred_native_path_string()
                            ),
                            source,
                        }));
                    }
                }
                let segment = candidate.basename().ok_or_else(|| {
                    ApplyPatchError::IoError(IoError {
                        context: format!(
                            "Failed to resolve mutation endpoint {}",
                            path.inferred_native_path_string()
                        ),
                        source: std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "no existing ancestor for mutation endpoint",
                        ),
                    })
                })?;
                missing_segments.push(segment);
                candidate = candidate.parent().ok_or_else(|| {
                    ApplyPatchError::IoError(IoError {
                        context: format!(
                            "Failed to resolve mutation endpoint {}",
                            path.inferred_native_path_string()
                        ),
                        source: std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "no existing ancestor for mutation endpoint",
                        ),
                    })
                })?;
            }
            Err(source) => {
                return Err(ApplyPatchError::IoError(IoError {
                    context: format!(
                        "Failed to resolve mutation endpoint {}",
                        path.inferred_native_path_string()
                    ),
                    source,
                }));
            }
        }
    };

    let windows = resolved.infer_path_convention() == Some(PathConvention::Windows);
    let sensitive = if windows && !missing_segments.is_empty() {
        fs.directory_case_sensitive(&resolved, sandbox)
            .await
            .ok()
            .flatten()
            == Some(true)
    } else {
        !windows
    };
    for (index, segment) in missing_segments.iter().rev().enumerate() {
        // Only the existing directory can prove its child-name policy. A
        // further missing directory has no observed policy yet; keep the
        // conservative duplicate-write guard there and for legacy providers.
        if windows && !(index == 0 && sensitive) {
            resolved = resolved.join(&segment.to_lowercase())?;
        } else {
            resolved = resolved.join(segment)?;
        }
    }

    // The selected provider supplied canonical spelling. Do not fold native
    // names or render opaque paths lossily to establish endpoint identity.
    Ok(resolved.to_string())
}

/// Extract the heredoc body (and optional `cd` workdir) from a `bash -lc` script
/// that invokes the apply_patch tool using a heredoc.
///
/// Supported top‑level forms (must be the only top‑level statement):
/// - `apply_patch <<'EOF'\n...\nEOF`
/// - `cd <path> && apply_patch <<'EOF'\n...\nEOF`
///
/// Notes about matching:
/// - Parsed with Tree‑sitter Bash and a strict query that uses anchors so the
///   heredoc‑redirected statement is the only top‑level statement.
/// - The connector between `cd` and `apply_patch` must be `&&` (not `|` or `||`).
/// - Exactly one literal path argument is allowed for `cd`, optionally quoted.
/// - The apply command is validated in‑query via `#any-of?` to allow `apply_patch`
///   or `applypatch`.
/// - Preceding or trailing commands (e.g., `echo ...;` or `... && echo done`) do not match.
///
/// Returns `(heredoc_body, Some(path))` when the `cd` variant matches, or
/// `(heredoc_body, None)` for the direct form. Errors are returned if the script
/// cannot be parsed or does not match the allowed patterns.
fn extract_apply_patch_from_bash(
    src: &str,
) -> std::result::Result<(String, Option<String>), ExtractHeredocError> {
    // Both accepted AST forms contain a literal command name. Reject ordinary
    // shell scripts before allocating a parser, including patch-scope probes.
    if !APPLY_PATCH_COMMANDS.iter().any(|name| src.contains(name)) {
        return Err(ExtractHeredocError::CommandDidNotStartWithApplyPatch);
    }
    // This function uses a Tree-sitter query to recognize one of two
    // whole-script forms, each expressed as a single top-level statement:
    //
    // 1. apply_patch <<'EOF'\n...\nEOF
    // 2. cd <path> && apply_patch <<'EOF'\n...\nEOF
    //
    // Key ideas when reading the query:
    // - dots (`.`) between named nodes enforces adjacency among named children and
    //   anchor to the start/end of the expression.
    // - we match a single redirected_statement directly under program with leading
    //   and trailing anchors (`.`). This ensures it is the only top-level statement
    //   (so prefixes like `echo ...;` or suffixes like `... && echo done` do not match).
    //
    // Overall, we want to be conservative and only match the intended forms, as other
    // forms are likely to be model errors, or incorrectly interpreted by later code.
    //
    // If you're editing this query, it's helpful to start by creating a debugging binary
    // which will let you see the AST of an arbitrary bash script passed in, and optionally
    // also run an arbitrary query against the AST. This is useful for understanding
    // how tree-sitter parses the script and whether the query syntax is correct. Be sure
    // to test both positive and negative cases.
    static APPLY_PATCH_QUERY: LazyLock<Query> = LazyLock::new(|| {
        let language = BASH.into();
        #[expect(clippy::expect_used)]
        Query::new(
            &language,
            r#"
            (
              program
                . (redirected_statement
                    body: (command
                            name: (command_name (word) @apply_name) .)
                    (#any-of? @apply_name "apply_patch" "applypatch")
                    redirect: (heredoc_redirect
                                . (heredoc_start)
                                . (heredoc_body) @heredoc
                                . (heredoc_end)
                                .))
                .)

            (
              program
                . (redirected_statement
                    body: (list
                            . (command
                                name: (command_name (word) @cd_name) .
                                argument: [
                                  (word) @cd_path
                                  (string . (string_content) @cd_path .)
                                  (raw_string) @cd_raw_string
                                ] .)
                            "&&"
                            . (command
                                name: (command_name (word) @apply_name) .)
                            .)
                    (#eq? @cd_name "cd")
                    (#any-of? @apply_name "apply_patch" "applypatch")
                    redirect: (heredoc_redirect
                                . (heredoc_start)
                                . (heredoc_body) @heredoc
                                . (heredoc_end)
                                .))
                .)
            "#,
        )
        .expect("valid bash query")
    });

    let lang = BASH.into();
    let mut parser = Parser::new();
    parser
        .set_language(&lang)
        .map_err(ExtractHeredocError::FailedToLoadBashGrammar)?;
    let tree = parser
        .parse(src, None)
        .ok_or(ExtractHeredocError::FailedToParsePatchIntoAst)?;

    let bytes = src.as_bytes();
    let root = tree.root_node();
    if root.has_error() {
        return Err(ExtractHeredocError::FailedToParsePatchIntoAst);
    }

    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&APPLY_PATCH_QUERY, root, bytes);
    while let Some(m) = matches.next() {
        let mut heredoc_text: Option<String> = None;
        let mut cd_path: Option<String> = None;

        for capture in m.captures() {
            let name = APPLY_PATCH_QUERY.capture_names()[capture.index as usize];
            match name {
                "heredoc" => {
                    let text = capture
                        .node
                        .utf8_text(bytes)
                        .map_err(ExtractHeredocError::HeredocNotUtf8)?
                        .trim_end_matches('\n')
                        .to_string();
                    heredoc_text = Some(text);
                }
                "cd_path" => {
                    let text = capture
                        .node
                        .utf8_text(bytes)
                        .map_err(ExtractHeredocError::HeredocNotUtf8)?
                        .to_string();
                    cd_path = Some(text);
                }
                "cd_raw_string" => {
                    let raw = capture
                        .node
                        .utf8_text(bytes)
                        .map_err(ExtractHeredocError::HeredocNotUtf8)?;
                    let trimmed = raw
                        .strip_prefix('\'')
                        .and_then(|s| s.strip_suffix('\''))
                        .unwrap_or(raw);
                    cd_path = Some(trimmed.to_string());
                }
                _ => {}
            }
        }

        if let Some(heredoc) = heredoc_text {
            return Ok((heredoc, cd_path));
        }
    }

    Err(ExtractHeredocError::CommandDidNotStartWithApplyPatch)
}
