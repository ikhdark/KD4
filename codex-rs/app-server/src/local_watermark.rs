use std::path::Path;

use codex_app_server_protocol::ServerLocalWatermark;

const LOCAL_WATERMARK_VERSION: &str = "kd4";
const LOCAL_WATERMARK_LABEL: &str = "Codex KD4";

pub(crate) async fn local_watermark(codex_home: &Path) -> ServerLocalWatermark {
    current(codex_home).await
}

async fn current(_codex_home: &Path) -> ServerLocalWatermark {
    ServerLocalWatermark {
        version: LOCAL_WATERMARK_VERSION.to_string(),
        label: LOCAL_WATERMARK_LABEL.to_string(),
        detail: "Local Codex KD4 build marker.".to_string(),
    }
}
