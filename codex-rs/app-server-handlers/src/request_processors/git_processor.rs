use super::ClientResponsePayload;
use super::GitDiffToRemoteParams;
use super::GitDiffToRemoteResponse;
use super::JSONRPCErrorError;
use super::Result;
use super::git_diff_to_remote;
use super::invalid_request;

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

