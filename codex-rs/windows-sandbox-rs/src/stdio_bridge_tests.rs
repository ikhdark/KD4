use std::sync::Mutex;

use pretty_assertions::assert_eq;

use super::*;

#[tokio::test]
async fn failed_native_write_or_flush_has_a_failed_delivery_receipt() {
    struct FailingWriter {
        remaining: usize,
        fail_flush: bool,
    }
    impl Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.remaining == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "injected",
                ));
            }
            let n = bytes.len().min(self.remaining);
            self.remaining -= n;
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            if self.fail_flush {
                Err(std::io::Error::other("injected flush failure"))
            } else {
                Ok(())
            }
        }
    }
    for writer in [
        FailingWriter {
            remaining: 3,
            fail_flush: false,
        },
        FailingWriter {
            remaining: 100,
            fail_flush: true,
        },
    ] {
        let (tx, rx) = mpsc::channel(2);
        tx.send(b"alpha".to_vec()).await.unwrap();
        tx.send(b"beta".to_vec()).await.unwrap();
        drop(tx);
        let (thread, receipt) =
            spawn_output_forwarder(tokio::runtime::Handle::current(), rx, writer);
        assert!(receipt.await.unwrap().is_err());
        thread.join().unwrap();
    }
    assert_eq!(
        StdioBridgeOutcome {
            child_exit_code: 0,
            delivery_errors: vec!["incomplete".into()]
        }
        .exit_code(),
        1
    );
    assert_eq!(
        StdioBridgeOutcome {
            child_exit_code: 17,
            delivery_errors: vec!["incomplete".into()]
        }
        .exit_code(),
        17
    );
}

#[tokio::test]
async fn failed_stdin_read_is_not_clean_eof() {
    struct FailedInput;
    impl Read for FailedInput {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("injected read failure"))
        }
    }
    let (tx, mut rx) = mpsc::channel(1);
    let (done, receipt) = oneshot::channel();
    let worker = spawn_input_forwarder(FailedInput, tx, done);
    assert!(receipt.await.unwrap().is_err());
    assert!(rx.recv().await.is_none());
    worker.join().unwrap();
}

#[tokio::test]
async fn input_forwarder_sends_chunks_and_reports_eof() -> anyhow::Result<()> {
    let (writer_tx, mut writer_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    let (stdin_closed_tx, stdin_closed_rx) = tokio::sync::oneshot::channel();
    let expected = (0..16_391)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    let input = std::io::Cursor::new(expected.clone());

    let forwarder = spawn_input_forwarder(input, writer_tx, stdin_closed_tx);
    let mut received = Vec::new();
    let mut chunk_lengths = Vec::new();
    while let Some(chunk) = writer_rx.recv().await {
        chunk_lengths.push(chunk.len());
        received.extend_from_slice(&chunk);
    }
    stdin_closed_rx.await??;
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
    tokio::time::timeout(Duration::from_secs(5), done_rx).await???;
    forwarder.join().expect("output forwarder should finish");

    let output = sink
        .lock()
        .map_err(|_| anyhow::anyhow!("writer poisoned"))?
        .clone();
    assert_eq!(output, b"alphabeta".to_vec());
    Ok(())
}
