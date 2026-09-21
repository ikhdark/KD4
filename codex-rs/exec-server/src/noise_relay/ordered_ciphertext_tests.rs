use pretty_assertions::assert_eq;

use super::MAX_PENDING_BYTES;
use super::OrderedCiphertextFrames;

#[test]
fn releases_ciphertexts_only_in_nonce_order() {
    let mut frames = OrderedCiphertextFrames::default();

    assert_eq!(
        frames.push(/*seq*/ 1, b"second".to_vec()).unwrap(),
        Vec::<Vec<u8>>::new()
    );
    assert_eq!(
        frames.push(/*seq*/ 0, b"first".to_vec()).unwrap(),
        vec![b"first".to_vec(), b"second".to_vec()]
    );
}

#[test]
fn ignores_duplicate_ciphertexts_without_replacing_buffered_record() {
    let mut frames = OrderedCiphertextFrames::default();

    assert_eq!(
        frames.push(/*seq*/ 1, b"first copy".to_vec()).unwrap(),
        Vec::<Vec<u8>>::new()
    );
    assert_eq!(
        frames.push(/*seq*/ 1, b"replacement".to_vec()).unwrap(),
        Vec::<Vec<u8>>::new()
    );
    assert_eq!(
        frames.push(/*seq*/ 0, b"zero".to_vec()).unwrap(),
        vec![b"zero".to_vec(), b"first copy".to_vec()]
    );
    assert_eq!(
        frames.push(/*seq*/ 0, b"duplicate".to_vec()).unwrap(),
        Vec::<Vec<u8>>::new()
    );
}

#[test]
fn rejects_unbounded_reordering() {
    let mut frames = OrderedCiphertextFrames::default();

    assert!(frames.push(/*seq*/ 65, Vec::new()).is_err());
    assert!(
        frames
            .push(/*seq*/ 1, vec![0; MAX_PENDING_BYTES + 1])
            .is_err()
    );
}

#[test]
fn aggregate_pending_budget_is_restored_after_release() {
    let mut frames = OrderedCiphertextFrames::default();
    let first = vec![1; MAX_PENDING_BYTES / 2];
    let second = vec![2; MAX_PENDING_BYTES / 2];
    assert!(frames.push(1, first.clone()).unwrap().is_empty());
    assert!(frames.push(2, second.clone()).unwrap().is_empty());
    assert!(frames.push(3, vec![3]).is_err());
    assert_eq!(
        frames.push(0, vec![0]).unwrap(),
        vec![vec![0], first, second]
    );
    let next = vec![4; MAX_PENDING_BYTES];
    assert!(frames.push(4, next.clone()).unwrap().is_empty());
    assert_eq!(frames.push(3, vec![3]).unwrap(), vec![vec![3], next]);
}

#[test]
fn accepts_exact_reorder_window() {
    let mut frames = OrderedCiphertextFrames::default();
    assert!(frames.push(64, vec![64]).unwrap().is_empty());
    for seq in 0..63 {
        assert_eq!(
            frames.push(seq, vec![seq as u8]).unwrap(),
            vec![vec![seq as u8]]
        );
    }
    assert_eq!(frames.push(63, vec![63]).unwrap(), vec![vec![63], vec![64]]);
}

#[tokio::test(start_paused = true)]
async fn gap_deadline_is_absolute_and_clears_only_on_recovery() {
    let mut frames = OrderedCiphertextFrames::default();
    assert_eq!(frames.gap_deadline(), None);
    frames.push(1, vec![1]).unwrap();
    let deadline = frames.gap_deadline().unwrap();
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    frames.push(3, vec![3]).unwrap();
    frames.push(1, vec![9]).unwrap();
    assert_eq!(frames.gap_deadline(), Some(deadline));
    assert_eq!(frames.push(0, vec![0]).unwrap(), vec![vec![0], vec![1]]);
    assert_eq!(frames.gap_deadline(), Some(deadline));
    assert_eq!(frames.push(2, vec![2]).unwrap(), vec![vec![2], vec![3]]);
    assert_eq!(frames.gap_deadline(), None);
}
