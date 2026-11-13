use super::*;

#[test]
fn rfc1459_folding_and_eq() {
    let cm = CaseMap::Rfc1459;
    assert!(cm.str_eq("[Admin]", "{admin}"));
    assert_eq!(cm.fold("[Admin]"), cm.fold("{admin}"));
    assert!(cm.str_eq("~nick\\1", "^NICK|1"));
    assert_eq!(cm.fold("~nick\\1"), cm.fold("^NICK|1"));
    let folded = cm.fold("[Alice]");
    assert_eq!(folded.as_str(), "{alice}");
    assert_eq!(folded.as_ref(), "{alice}");
    assert_eq!(folded.to_string(), "{alice}");
}

#[test]
fn strict_rfc1459_omits_tilde() {
    let cm = CaseMap::StrictRfc1459;
    assert!(cm.str_eq("[Admin]\\x", "{admin}|X"));
    assert_eq!(cm.fold("[Admin]\\x"), cm.fold("{admin}|X"));
    assert!(!cm.str_eq("~nick", "^nick"));
    assert_ne!(cm.fold("~nick"), cm.fold("^nick"));
}

#[test]
fn ascii_only_folds_alpha() {
    let cm = CaseMap::Ascii;
    assert!(cm.str_eq("Alice", "alice"));
    assert!(!cm.str_eq("[a]", "{a}"));
    assert_ne!(cm.fold("[a]"), cm.fold("{a}"));
    assert!(!cm.str_eq("~a", "^a"));
    assert!(!cm.str_eq("Alice", "Alice2"));
}

#[test]
fn isupport_parsing_and_u8_roundtrip() {
    assert_eq!(CaseMap::default(), CaseMap::Rfc1459);
    assert_eq!(CaseMap::from_isupport("rfc1459"), Some(CaseMap::Rfc1459));
    assert_eq!(
        CaseMap::from_isupport("strict-rfc1459"),
        Some(CaseMap::StrictRfc1459)
    );
    assert_eq!(CaseMap::from_isupport("ASCII"), Some(CaseMap::Ascii));
    assert_eq!(CaseMap::from_isupport("unknown"), None);

    for cm in [CaseMap::Rfc1459, CaseMap::StrictRfc1459, CaseMap::Ascii] {
        assert_eq!(CaseMap::from_u8(cm.to_u8()), cm);
    }
    assert_eq!(CaseMap::from_u8(255), CaseMap::Rfc1459);
}
