use super::*;
use std::future::pending;

#[tokio::test]
async fn a_completed_dial_passes_through_untouched() {
    let ok = with_dial_timeout("mock", async { Ok(7u8) }).await.unwrap();
    assert_eq!(ok, 7);
}

#[tokio::test]
async fn a_failed_dial_keeps_its_own_error() {
    let err = with_dial_timeout::<_, ()>("mock", async {
        Err(MeowError::Proxy("connection refused".into()))
    })
    .await
    .unwrap_err();
    assert!(matches!(err, MeowError::Proxy(_)), "got {err:?}");
}

/// A server that accepts and then never speaks must not park the caller.
#[tokio::test(start_paused = true)]
async fn a_stalled_dial_expires_as_timed_out() {
    let start = tokio::time::Instant::now();
    let err = with_dial_timeout::<_, ()>("🇭🇰 HK-01", pending())
        .await
        .unwrap_err();

    let MeowError::Io(io_err) = err else {
        panic!("expected an io error, got {err:?}");
    };
    assert_eq!(io_err.kind(), io::ErrorKind::TimedOut);
    assert!(io_err.to_string().contains("🇭🇰 HK-01"));
    assert_eq!(start.elapsed(), DIAL_TIMEOUT);
}

/// The bound is a ceiling, not a delay: a dial that finishes just under it
/// is not slowed down, and one that finishes just over it is cut.
#[tokio::test(start_paused = true)]
async fn the_bound_is_exclusive_to_slow_dials() {
    let just_in_time = with_dial_timeout("mock", async {
        tokio::time::sleep(DIAL_TIMEOUT - Duration::from_millis(1)).await;
        Ok(())
    })
    .await;
    assert!(just_in_time.is_ok());

    let too_slow = with_dial_timeout("mock", async {
        tokio::time::sleep(DIAL_TIMEOUT + Duration::from_millis(1)).await;
        Ok(())
    })
    .await;
    assert!(too_slow.is_err());
}
#[tokio::test(start_paused = true)]
async fn nested_dials_share_the_original_budget() {
    let start = Instant::now();
    with_dial_timeout::<_, ()>("outer", async {
        tokio::time::sleep(Duration::from_secs(3)).await;
        with_dial_timeout("inner", async {
            assert_eq!(dial_deadline(), Some(start + DIAL_TIMEOUT));
            pending().await
        })
        .await
    })
    .await
    .unwrap_err();
    assert_eq!(start.elapsed(), DIAL_TIMEOUT);
    assert!(
        dial_deadline().is_none(),
        "deadline must not leak out of its scope"
    );
}
