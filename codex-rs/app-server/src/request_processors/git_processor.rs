use super::*;

pub(crate) async fn git_diff_to_remote_response(
    params: GitDiffToRemoteParams,
) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
    let cwd = params.cwd;
    git_diff_to_remote(&cwd)
        .await
        .map(|value| {
            Some(
                GitDiffToRemoteResponse {
                    sha: value.sha,
                    diff: value.diff,
                }
                .into(),
            )
        })
        .ok_or_else(|| {
            invalid_request(format!(
                "failed to compute git diff to remote for cwd: {cwd:?}"
            ))
        })
}

#[cfg(test)]
#[path = "git_processor_tests.rs"]
mod tests;
