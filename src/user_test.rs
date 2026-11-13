use std::collections::{BTreeSet, HashSet};

use super::*;

#[test]
fn rfc1459_identity_rename_and_quit_rejoin() {
    let reg = UserRegistry::new(CaseMap::Rfc1459);
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
    let (quit_track, netsplit) = reg
        .mark_quit("Alice2", 42, Some("hub.eu.net leaf.us.net".into()), true)
        .unwrap();
    assert_eq!(quit_track.id, t1.id);
    assert!(netsplit);
    assert!(!u1.is_online());
    assert_eq!(
        t1.status(),
        UserStatus::Quit {
            at_seq: 42,
            nick: "Alice2".into(),
            reason: Some("hub.eu.net leaf.us.net".into()),
            netsplit: true,
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
fn netsplit_detection_and_account_transitions() {
    assert!(is_netsplit_reason("hub.eu.net leaf.us.net"));
    assert!(is_netsplit_reason("*.net *.split"));
    assert!(!is_netsplit_reason("Quit: Leaving"));
    assert!(!is_netsplit_reason("Ping timeout: 240 seconds"));
    assert!(!is_netsplit_reason("single.server.net"));
    assert!(!is_netsplit_reason("1.2 3.4"));

    let reg = UserRegistry::default();
    let u = reg.get_or_create("Alice");
    assert_eq!(u.account(), None);

    reg.update_account("alice", "alice_acct", false);
    assert_eq!(u.account().as_deref(), Some("alice_acct"));

    reg.update_account("ALICE", "*", false);
    assert_eq!(u.account(), None);

    reg.update_account("ALICE", "0", false);
    assert_eq!(u.account(), None);
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
    let reg = UserRegistry::new(CaseMap::Rfc1459);
    let a = reg.get_or_create("[Alice]");
    assert_eq!(reg.get_or_create("{alice}").id, a.id);

    reg.rekey_casemap(CaseMap::Ascii);
    assert_eq!(a.casemap(), CaseMap::Ascii);
    assert_eq!(reg.get_or_create("[alice]").id, a.id);
    let b = reg.get_or_create("{alice}");
    assert_ne!(b.id, a.id);
}
