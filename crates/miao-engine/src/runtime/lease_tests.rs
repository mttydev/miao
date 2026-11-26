use super::*;

#[test]
fn a_renewed_claim_keeps_the_turn() {
    assert_eq!(renew_decision(&Ok(()), 0), Renewal::Keep);
    assert_eq!(renew_decision(&Ok(()), 4), Renewal::Keep);
}

#[test]
fn a_fenced_claim_stops_the_turn_and_names_why() {
    // Another writer owns the Session now: every later write is refused by the
    // fence, so stopping is right and the journal has to carry the reason.
    assert_eq!(
        renew_decision(&Err(Error::Fenced), 1),
        Renewal::Stop("lease_lost")
    );
}

#[test]
fn a_contended_store_does_not_end_the_turn() {
    // A `busy` timeout behind another window's write transaction is a stall,
    // not a lost claim. Observed on ledgers this product grows: a write lock
    // held for seconds turns into an interrupted turn with no explanation.
    assert_eq!(renew_decision(&Err(Error::Busy), 1), Renewal::Keep);
    assert_eq!(
        renew_decision(&Err(Error::Io(std::io::Error::other("stalled"))), 1),
        Renewal::Keep
    );
    assert_eq!(
        renew_decision(&Err(Error::Busy), LEASE_RENEWAL_PATIENCE - 1),
        Renewal::Keep
    );
}

#[test]
fn a_claim_that_stopped_renewing_past_its_window_stops_the_turn() {
    assert_eq!(
        renew_decision(&Err(Error::Busy), LEASE_RENEWAL_PATIENCE),
        Renewal::Stop("lease_unavailable")
    );
}

#[test]
fn the_patience_spans_two_lease_intervals() {
    // The claim lapses at two intervals without renewal, so the drain must stop
    // by then: thirty failures two seconds apart is exactly that window.
    assert_eq!(
        LEASE_RENEWAL_PATIENCE * 250 * LEASE_RENEWAL_TICKS,
        (2 * SESSION_LEASE_MS) as u32
    );
}
