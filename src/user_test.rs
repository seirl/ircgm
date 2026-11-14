use std::collections::{BTreeSet, HashSet};

use super::*;

#[test]
fn rfc1459_identity_rename_and_quit_rejoin() {
    let reg = UserRegistry::new(AtomicCaseMap::new(CaseMap::Rfc1459));
    let t1 = reg.get_or_create("[Alice]");
    let t2 = reg.get_or_create("{alice}");
    assert_eq!(t1.id, t2.id);

    let u1 = User::from_parts(Weak::new(), Arc::clone(&t1));
    let u2 = User::from_parts(Weak::new(), Arc::clone(&t2));
    assert_eq!(u1, u2);
    assert_eq!(u1.id(), u2.id());
    assert_eq!(u1.id().as_u64(), t1.id);
    assert_eq!(u1.id().to_string(), t1.id.to_string());
    assert!(u1.eq_nick("{ALICE}"));

    // Survives /nick rename:
    let renamed = reg.rename("{alice}", "Alice2");
    assert_eq!(renamed.id, t1.id);
    assert_eq!(u1.nick(), "Alice2");
    assert!(reg.get_existing("[Alice]").is_none());
    assert_eq!(reg.get_or_create("alice2").id, t1.id);

    // QUIT + rejoin allocates a fresh UserTrack::id / UserId:
    let quit_track = reg
        .mark_quit("Alice2", 42, Some("hub.eu.net leaf.us.net".into()))
        .unwrap();
    assert_eq!(quit_track.id, t1.id);
    assert!(!u1.is_online());
    assert_eq!(
        t1.status(),
        UserStatus::Quit {
            at_seq: 42,
            nick: "Alice2".into(),
            reason: Some("hub.eu.net leaf.us.net".into()),
        }
    );

    let rejoined = reg.get_or_create("Alice2");
    let u_rejoined = User::from_parts(Weak::new(), rejoined);
    assert_ne!(u_rejoined, u1);
    assert_ne!(u_rejoined.id(), u1.id());
    assert!(u_rejoined.is_online());

    let mut hash_ids = HashSet::new();
    assert!(hash_ids.insert(u1.id()));
    assert!(hash_ids.insert(u_rejoined.id()));
    let mut ord_ids = BTreeSet::new();
    ord_ids.insert(u_rejoined.id());
    ord_ids.insert(u1.id());
    assert_eq!(ord_ids.len(), 2);
}

#[test]
fn account_transitions() {
    let reg = UserRegistry::default();
    let u = reg.get_or_create("Alice");
    assert_eq!(u.account(), None);

    reg.update_account("alice", "alice_acct");
    assert_eq!(u.account().as_deref(), Some("alice_acct"));

    reg.update_account("ALICE", "*");
    assert_eq!(u.account(), None);

    reg.update_account("ALICE", "0");
    assert_eq!(u.account(), None);
}

#[test]
fn resolve_send_error_distinguishes_401_departure_from_other_bounces() {
    let reg = UserRegistry::new(AtomicCaseMap::new(CaseMap::Rfc1459));
    let alice = reg.get_or_create("[Alice]");

    // Non-401 bounce resolves `alice.id` without marking offline:
    assert_eq!(
        reg.resolve_send_error("{alice}", 716, 10, "callerid"),
        (Some(alice.id), None)
    );
    assert!(alice.is_online());

    // 401 marks `alice` offline and returns `quit_track_id`:
    assert_eq!(
        reg.resolve_send_error("{alice}", 401, 11, "No such nick"),
        (Some(alice.id), Some(alice.id))
    );
    assert!(!alice.is_online());
}

#[test]
fn amortized_dead_weak_gc_without_drop_reentrancy() {
    let reg = UserRegistry::default();
    for i in 0..65 {
        let _temp = reg.get_or_create(&format!("Transient{i}"));
    }
    let _keep = reg.get_or_create("Survivor");
    assert_eq!(reg.tracked_map_len(), 1);
}

#[test]
fn dynamic_casemapping_rekey() {
    let shared = AtomicCaseMap::new(CaseMap::Rfc1459);
    let reg = UserRegistry::new(shared.clone());
    let a = reg.get_or_create("[Alice]");
    assert_eq!(reg.get_or_create("{alice}").id, a.id);

    // Simulate `WorldState::apply_isupport` flipping the shared `AtomicCaseMap` BEFORE
    // `UserRegistry::rekey_casemap` runs: concurrent lookups must keep using `state.casemap`
    // (`Rfc1459`) so `[Alice]`'s `UserTrack` is never split!
    shared.store(CaseMap::Ascii);
    assert_eq!(reg.get_or_create("[Alice]").id, a.id);
    assert_eq!(reg.get_existing("[alice]").map(|t| t.id), Some(a.id));

    reg.rekey_casemap(CaseMap::Ascii);
    assert_eq!(a.casemap(), CaseMap::Ascii);
    assert_eq!(reg.get_or_create("[alice]").id, a.id);
    let b = reg.get_or_create("{alice}");
    assert_ne!(b.id, a.id);
}
