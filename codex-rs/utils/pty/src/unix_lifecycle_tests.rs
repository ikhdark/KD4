use super::*;

async fn shell(script: &str) -> SpawnedProcess {
    spawn_process(
        "/bin/sh",
        &["-c".into(), script.into()],
        &std::env::current_dir().unwrap(),
        &std::env::vars().collect(),
        &None,
        TerminalSize::default(),
    )
    .await
    .unwrap()
}

async fn ready(output: &mut mpsc::Receiver<Vec<u8>>) {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut bytes = Vec::new();
        while !bytes.windows(5).any(|window| window == b"READY") {
            bytes.extend(output.recv().await.expect("terminal readiness"));
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn ordered_terminal_eof_uses_custom_veof_after_a_partial_line() {
    let SpawnedProcess {
        session,
        mut stdout_rx,
        exit_rx,
        ..
    } = shell("stty -echo eof '^A'; printf READY; cat").await;
    ready(&mut stdout_rx).await;
    session
        .writer_sender()
        .send(b"partial-nonce".to_vec())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), session.close_stdin_ordered())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), exit_rx)
            .await
            .unwrap()
            .unwrap(),
        0
    );
    let output = tokio::time::timeout(Duration::from_secs(5), async {
        let mut bytes = Vec::new();
        while let Some(chunk) = stdout_rx.recv().await {
            bytes.extend(chunk);
        }
        bytes
    })
    .await
    .unwrap();
    assert_eq!(output, b"partial-nonce");
    assert!(session.output_capture().stdout_complete());
    // An acknowledged close is idempotent and cannot inject more input.
    session.close_stdin_ordered().await.unwrap();
}

#[tokio::test]
async fn raw_terminal_close_is_not_falsely_acknowledged_as_eof() {
    let SpawnedProcess {
        session,
        mut stdout_rx,
        exit_rx,
        ..
    } = shell("stty raw -echo; printf READY; cat").await;
    ready(&mut stdout_rx).await;
    let error = tokio::time::timeout(Duration::from_secs(5), session.close_stdin_ordered())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    assert!(
        !session.has_exited(),
        "unsupported EOF must not cancel the terminal"
    );
    session.request_terminate().unwrap();
    tokio::time::timeout(Duration::from_secs(5), exit_rx)
        .await
        .unwrap()
        .unwrap();
    session.finish();
}
