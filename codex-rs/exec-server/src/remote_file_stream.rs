use bytes::Bytes;
use codex_utils_path_uri::PathUri;
use std::sync::Arc;
use tokio::io;
use uuid::Uuid;

use super::map_remote_error;
use crate::ExecServerClient;
use crate::FILE_READ_CHUNK_SIZE;
use crate::FileSystemReadStream;
use crate::FileSystemResult;
use crate::FileSystemSandboxContext;
use crate::protocol::FS_CLOSE_METHOD;
use crate::protocol::FS_OPEN_METHOD;
use crate::protocol::FS_READ_BLOCK_METHOD;
use crate::protocol::FsCloseParams;
use crate::protocol::FsCloseResponse;
use crate::protocol::FsOpenParams;
use crate::protocol::FsOpenResponse;
use crate::protocol::FsReadBlockParams;
use crate::protocol::FsReadBlockResponse;
use crate::rpc::RpcClient;

struct FileReadRegistration {
    client: ExecServerClient,
    rpc_client: Arc<RpcClient>,
    handle_id: String,
    cleanup_slot: Option<tokio::sync::OwnedSemaphorePermit>,
}

pub(super) async fn open(
    client: ExecServerClient,
    path: PathUri,
    sandbox: Option<FileSystemSandboxContext>,
) -> FileSystemResult<FileSystemReadStream> {
    // Hold admission through deferred close, bounding both open handles and
    // cleanup tasks even when a caller rapidly consumes many small files.
    let cleanup_slot = client
        .reserve_file_stream()
        .await
        .map_err(map_remote_error)?;
    let rpc_client = client.rpc_client().await.map_err(map_remote_error)?;
    let registration = FileReadRegistration {
        client,
        rpc_client,
        handle_id: Uuid::new_v4().simple().to_string(),
        cleanup_slot: Some(cleanup_slot),
    };
    let _: FsOpenResponse = registration
        .client
        .call_rpc(
            &registration.rpc_client,
            FS_OPEN_METHOD,
            &FsOpenParams {
                handle_id: registration.handle_id.clone(),
                path,
                sandbox,
            },
        )
        .await
        .map_err(map_remote_error)?;
    Ok(FileSystemReadStream::new(futures::stream::try_unfold(
        Some((registration, 0_u64)),
        |state| async move {
            let Some((registration, offset)) = state else {
                return Ok(None);
            };
            let response: FsReadBlockResponse = registration
                .client
                .call_rpc(
                    &registration.rpc_client,
                    FS_READ_BLOCK_METHOD,
                    &FsReadBlockParams {
                        handle_id: registration.handle_id.clone(),
                        offset,
                        len: FILE_READ_CHUNK_SIZE,
                    },
                )
                .await
                .map_err(map_remote_error)?;
            let chunk = Bytes::from(response.chunk.into_inner());
            if chunk.len() > FILE_READ_CHUNK_SIZE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{FS_READ_BLOCK_METHOD} returned {} bytes, maximum is {}",
                        chunk.len(),
                        FILE_READ_CHUNK_SIZE
                    ),
                ));
            }
            if response.eof {
                // Drop transfers the reservation to the existing bounded close
                // path; already-received bytes need not wait for its response.
                drop(registration);
                return if chunk.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some((chunk, None)))
                };
            }
            if chunk.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{FS_READ_BLOCK_METHOD} returned an empty non-terminal block"),
                ));
            }
            let next_offset = offset.checked_add(chunk.len() as u64).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{FS_READ_BLOCK_METHOD} offset overflowed after {offset} bytes"),
                )
            })?;
            Ok(Some((chunk, Some((registration, next_offset)))))
        },
    )))
}

impl Drop for FileReadRegistration {
    fn drop(&mut self) {
        let cleanup_slot = self.cleanup_slot.take();
        let rpc_client = Arc::clone(&self.rpc_client);
        let handle_id = self.handle_id.clone();
        let runtime = rpc_client.cleanup_runtime();
        let deadline = tokio::time::Instant::now() + crate::client::PROCESS_TERMINATION_TIMEOUT;
        // Prearm before spawn so a stopped runtime dropping an unpolled future
        // still closes the exact originating transport.
        let fallback = CloseFallback(Some(Arc::clone(&rpc_client)));
        runtime.spawn(async move {
                let mut fallback = fallback;
                let _cleanup_slot = cleanup_slot;
                let _: Result<FsCloseResponse, _> = ExecServerClient::call_for_cleanup_on_before(
                    &rpc_client,
                    FS_CLOSE_METHOD,
                    &FsCloseParams { handle_id },
                    deadline,
                )
                .await;
                fallback.0 = None; // The cleanup helper closes transport on failure.
        });
    }
}

struct CloseFallback(Option<Arc<RpcClient>>);

impl Drop for CloseFallback {
    fn drop(&mut self) {
        if let Some(rpc) = self.0.take() { rpc.request_transport_close(); }
    }
}
