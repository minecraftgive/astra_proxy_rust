use super::*;
use std::sync::Arc;

/// Build a pool-typed snell stream over an in-memory duplex. The peer
/// half is returned so tests can keep the underlying stream open (or
/// wrap it in a `V4Conn` to speak the AEAD protocol from the far side).
fn make_stream() -> (PoolStream, tokio::io::DuplexStream) {
    let (a, b) = tokio::io::duplex(1 << 16);
    (
        Snell::new(
            Box::new(a) as Box<dyn TransportStream>,
            Arc::from(b"k".as_slice()),
        ),
        b,
    )
}

#[test]
fn take_idle_on_empty_pool_is_none() {
    let pool = Pool::new();
    assert!(pool.take_idle().is_none());
}

#[test]
fn pool_is_lifo() {
    let pool = Pool::new();
    // The older conn's peer is gone, so the two are told apart by
    // whether the conn still looks alive.
    let (older, peer_older) = make_stream();
    drop(peer_older);
    let (newer, _peer_newer) = make_stream();
    pool.put(older, pool.generation());
    pool.put(newer, pool.generation());

    let mut conn = pool.take_idle().expect("first take should pop an entry");
    assert!(
        conn.idle_conn_alive(),
        "most recently returned conn comes back first"
    );
    let mut conn = pool.take_idle().expect("second take should pop an entry");
    assert!(!conn.idle_conn_alive());
    assert!(pool.take_idle().is_none());
}

#[test]
fn put_keeps_a_conn_after_any_number_of_sessions() {
    let pool = Pool::new();
    let (conn, _peer) = make_stream();
    pool.put(conn, pool.generation());
    for session in 0..8 {
        let conn = pool
            .take_idle()
            .unwrap_or_else(|| panic!("session {session} found no pooled conn"));
        pool.put(conn, pool.generation());
    }
    assert_eq!(pool.idle_count(), 1);
}

#[test]
fn custom_limits_apply() {
    let pool = Pool::with_limits(1, Duration::from_secs(60));
    let (first, _peer_first) = make_stream();
    pool.put(first, pool.generation());
    let (extra, _peer_extra) = make_stream();
    pool.put(extra, pool.generation());
    assert_eq!(pool.idle_count(), 1, "max_size caps idle entries");
    assert!(pool.take_idle().is_some());

    let expired = Pool::with_limits(4, Duration::ZERO);
    let (conn, _peer) = make_stream();
    expired.put(conn, expired.generation());
    assert!(
        expired.take_idle().is_none(),
        "zero max_age expires at once"
    );
}

#[test]
fn put_respects_max_size() {
    let pool = Pool::new();
    let mut peers = Vec::new();
    for _ in 0..11 {
        let (conn, peer) = make_stream();
        peers.push(peer);
        pool.put(conn, pool.generation());
    }
    for i in 0..10 {
        assert!(
            pool.take_idle().is_some(),
            "take {i} should pop a pooled conn"
        );
    }
    assert!(pool.take_idle().is_none(), "pool is capped at 10 entries");
}

/// Issue #695: a reset drops idle conns and refuses conns checked out
/// (or dialled) before it, while post-reset conns pool normally.
#[test]
fn reset_drops_idle_and_refuses_stale_returns() {
    let pool = Pool::new();
    let (idle, _peer_idle) = make_stream();
    pool.put(idle, pool.generation());
    let checked_out_at = pool.generation();
    let (busy, _peer_busy) = make_stream();

    assert_eq!(pool.reset(), 1, "the idle conn is dropped");
    assert_eq!(pool.idle_count(), 0);
    pool.put(busy, checked_out_at);
    assert_eq!(pool.idle_count(), 0, "a pre-reset conn is never re-pooled");

    let (fresh, _peer_fresh) = make_stream();
    pool.put(fresh, pool.generation());
    assert_eq!(pool.idle_count(), 1, "post-reset conns pool normally");
    assert_eq!(pool.reset(), 1);
}
