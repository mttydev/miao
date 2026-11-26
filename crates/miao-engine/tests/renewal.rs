//! A lease renewal is what keeps a running turn alive, so it must survive the
//! store being busy. The old implementation used the shared writer connection
//! and the store's five second busy window, and `keep_lease` turned any error
//! into a cancelled turn: one long write transaction elsewhere ended every turn
//! in every window at once, with nothing written about why.

use miao_engine::{
    protocol::{Delivery, Input},
    store::Store,
};

fn input(id: &str) -> Input {
    Input {
        agent_references: Default::default(),
        command: None,
        session_id: "s".into(),
        input_id: id.into(),
        prompt: id.into(),
        delivery: Delivery::Steer,
    }
}

#[tokio::test]
async fn a_renewal_waits_out_a_write_lock_instead_of_losing_the_claim() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("engine.db");
    let store = Store::open(&path).await.unwrap();
    store.admit(input("one")).await.unwrap();
    let lease = store.claim_session("s", 30_000).await.unwrap();

    // Another writer holds the ledger longer than the old five second window.
    let holder = rusqlite::Connection::open(&path).unwrap();
    holder
        .busy_timeout(std::time::Duration::from_secs(10))
        .unwrap();
    holder.execute_batch("BEGIN IMMEDIATE;").unwrap();

    let renewing = tokio::spawn({
        let store = store.clone();
        let lease = lease.clone();
        async move { store.renew_session(&lease, 30_000).await }
    });
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
    holder.execute_batch("ROLLBACK;").unwrap();

    let renewed = renewing.await.unwrap();
    assert!(
        renewed.is_ok(),
        "a renewal must wait out a stall instead of losing the claim: {:?}",
        renewed.err()
    );
    assert_eq!(renewed.unwrap().epoch, lease.epoch);
}

#[tokio::test]
async fn a_renewal_is_fenced_only_when_another_writer_claims_the_session() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("engine.db");
    let store = Store::open(&path).await.unwrap();
    store.admit(input("one")).await.unwrap();
    let lease = store.claim_session("s", 30_000).await.unwrap();
    let takeover = store.claim_session("s", 30_000).await.unwrap();
    assert!(takeover.epoch > lease.epoch);
    assert!(matches!(
        store.renew_session(&lease, 30_000).await,
        Err(miao_engine::protocol::Error::Fenced)
    ));
}

#[tokio::test]
async fn the_wal_is_left_alone_while_it_is_small() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("engine.db");
    let store = Store::open(&path).await.unwrap();
    store.admit(input("one")).await.unwrap();
    store.checkpoint_wal().await.unwrap();
    // Nothing to merge: the frames stay in the WAL and every reader follows
    // them there, so a small ledger never pays for a checkpoint.
    assert!(path.with_file_name("engine.db-wal").exists());
}
