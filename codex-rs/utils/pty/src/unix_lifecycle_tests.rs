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

#[cfg(target_os = "linux")]
#[tokio::test]
async fn ordered_terminal_close_drains_input_without_synthesizing_control_bytes() {
    for (settings, payload) in [
        ("-echo -opost icanon eof '^A'", &b"accepted\n"[..]),
        (
            "-echo -opost icanon iexten lnext '^V' eof '^D'",
            &b"accepted\n\x16"[..],
        ),
        (
            "-echo -opost icanon isig intr '^C' eof '^C'",
            &b"accepted\n"[..],
        ),
        ("raw -echo -opost", &b"accepted\n"[..]),
    ] {
        let SpawnedProcess {
            session,
            mut stdout_rx,
            mut exit_rx,
            ..
        } = shell(&format!("stty {settings}; printf READY; cat")).await;
        ready(&mut stdout_rx).await;
        session
            .writer_sender()
            .send(payload.to_vec())
            .await
            .unwrap();
        let first = tokio::time::timeout(Duration::from_secs(5), session.close_stdin_ordered())
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(first.kind(), ErrorKind::Unsupported, "{settings}");
        let second = session.close_stdin_ordered().await.unwrap_err();
        assert_eq!(
            (second.kind(), second.to_string()),
            (first.kind(), first.to_string())
        );
        let output = tokio::time::timeout(Duration::from_secs(5), async {
            let mut output = Vec::new();
            while output.len() < b"accepted\n".len() {
                output.extend(stdout_rx.recv().await.expect("accepted output"));
            }
            output
        })
        .await
        .unwrap();
        assert_eq!(output, b"accepted\n", "{settings}");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), stdout_rx.recv())
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut exit_rx)
                .await
                .is_err()
        );
        session.request_terminate().unwrap();
        tokio::time::timeout(Duration::from_secs(5), exit_rx)
            .await
            .unwrap()
            .unwrap();
        session.finish();
    }
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
