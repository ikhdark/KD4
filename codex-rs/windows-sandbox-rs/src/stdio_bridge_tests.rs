use std::sync::Mutex;

use pretty_assertions::assert_eq;

use super::*;

#[tokio::test]
async fn input_forwarder_sends_chunks_and_reports_eof() -> anyhow::Result<()> {
    let (writer_tx, mut writer_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    let (stdin_closed_tx, stdin_closed_rx) = tokio::sync::oneshot::channel();
    let expected = (0..16_391).map(|index| (index % 251) as u8).collect::<Vec<_>>();
    let input = std::io::Cursor::new(expected.clone());

    let forwarder = spawn_input_forwarder(input, writer_tx, stdin_closed_tx);
    let mut received = Vec::new();
    let mut chunk_lengths = Vec::new();
    while let Some(chunk) = writer_rx.recv().await {
        chunk_lengths.push(chunk.len());
        received.extend_from_slice(&chunk);
    }
    stdin_closed_rx.await?;
    forwarder.join().expect("stdin forwarder should finish");

    assert_eq!(chunk_lengths, vec![8192, 8192, 7]);
    assert_eq!(received, expected);
    Ok(())
}

#[tokio::test]
async fn output_forwarder_writes_all_chunks() -> anyhow::Result<()> {
    #[derive(Clone, Default)]
    struct SharedWriter(std::sync::Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut guard = self
                .0
                .lock()
                .map_err(|_| std::io::Error::other("writer poisoned"))?;
            let written = buf.len().min(2);
            guard.extend_from_slice(&buf[..written]);
            Ok(written)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let runtime = tokio::runtime::Handle::current();
    let (output_tx, output_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    let writer = SharedWriter::default();
    let sink = std::sync::Arc::clone(&writer.0);

    let (forwarder, done_rx) = spawn_output_forwarder(runtime, output_rx, writer);
    output_tx.send(b"alpha".to_vec()).await?;
    output_tx.send(b"beta".to_vec()).await?;
    drop(output_tx);
    tokio::time::timeout(Duration::from_secs(5), done_rx).await??;
    forwarder.join().expect("output forwarder should finish");

    let output = sink
        .lock()
        .map_err(|_| anyhow::anyhow!("writer poisoned"))?
        .clone();
    assert_eq!(output, b"alphabeta".to_vec());
    Ok(())
}
