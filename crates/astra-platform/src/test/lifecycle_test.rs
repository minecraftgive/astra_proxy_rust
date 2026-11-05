use super::*;
#[test]
fn current_process_identity_is_stable_and_rejects_reused_pid() {
    let mut current = capture(std::process::id()).unwrap();
    assert!(matches_live(&current).unwrap());
    assert_eq!(
        identity(None).unwrap().boot_id,
        identity(None).unwrap().boot_id
    );
    current.started.push('x');
    assert!(!matches_live(&current).unwrap());
}
#[tokio::test]
async fn candidate_owner_loss_is_fatal_until_commit() {
    let mut dead = capture(std::process::id()).unwrap();
    dead.started.push('x');
    let candidate = Candidate {
        owner: Some(dead),
        committed: std::sync::atomic::AtomicBool::new(false),
    };
    assert!(candidate.commit().is_err());
    assert!(candidate
        .wait_owner_loss()
        .await
        .to_string()
        .contains("before service commitment"));
    let candidate = Candidate {
        owner: Some(capture(std::process::id()).unwrap()),
        committed: std::sync::atomic::AtomicBool::new(false),
    };
    candidate.commit().unwrap();
    assert!(tokio::time::timeout(
        std::time::Duration::from_millis(20),
        candidate.wait_owner_loss()
    )
    .await
    .is_err());
}
