use std::fmt;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

/// Server-negotiated IRC case-folding rule (`005 CASEMAPPING=rfc1459 | strict-rfc1459 | ascii`;
/// default `Rfc1459`).
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CaseMap {
    #[default]
    Rfc1459 = 0,
    StrictRfc1459 = 1,
    Ascii = 2,
}

impl CaseMap {
    pub(crate) const fn to_u8(self) -> u8 {
        self as u8
    }

    pub(crate) const fn from_u8(val: u8) -> Self {
        match val {
            1 => CaseMap::StrictRfc1459,
            2 => CaseMap::Ascii,
            _ => CaseMap::Rfc1459,
        }
    }

    /// Parses an `005 RPL_ISUPPORT` `CASEMAPPING=<value>` token.
    pub(crate) fn from_isupport(value: &str) -> Option<Self> {
        if value.eq_ignore_ascii_case("rfc1459") {
            Some(CaseMap::Rfc1459)
        } else if value.eq_ignore_ascii_case("strict-rfc1459") {
            Some(CaseMap::StrictRfc1459)
        } else if value.eq_ignore_ascii_case("ascii") {
            Some(CaseMap::Ascii)
        } else {
            None
        }
    }

    #[inline]
    fn fold_char(self, c: char) -> char {
        if c.is_ascii() {
            self.fold_byte(c as u8) as char
        } else {
            c
        }
    }

    #[inline]
    fn fold_byte(self, b: u8) -> u8 {
        match self {
            CaseMap::Rfc1459 => match b {
                b'A'..=b'Z' => b + 32,
                b'[' => b'{',
                b']' => b'}',
                b'\\' => b'|',
                b'~' => b'^',
                _ => b,
            },
            CaseMap::StrictRfc1459 => match b {
                b'A'..=b'Z' => b + 32,
                b'[' => b'{',
                b']' => b'}',
                b'\\' => b'|',
                _ => b,
            },
            CaseMap::Ascii => b.to_ascii_lowercase(),
        }
    }

    /// Case-folds `s` according to `self`, returning a canonical [`FoldedStr`].
    pub fn fold(self, s: &str) -> FoldedStr {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            out.push(self.fold_char(c));
        }
        FoldedStr(out)
    }

    /// Allocation-free case-folded string equality (`str_eq` avoids shadowing `PartialEq::eq` on `CaseMap`).
    pub fn str_eq(self, a: &str, b: &str) -> bool {
        a.len() == b.len()
            && a.bytes()
                .zip(b.bytes())
                .all(|(x, y)| self.fold_byte(x) == self.fold_byte(y))
    }
}

/// Shared lock-free [`CaseMap`] cell owned by `BotCore` and shared across `WorldState`,
/// `UserRegistry`, `UserTrack`, `InboxRegistry`, and `OutboundSink`.
///
/// Concurrency invariant: structures that key `HashMap<FoldedStr, _>` (`WorldState`,
/// `UserRegistry`, `InboxRegistry`) ALSO keep a plain `CaseMap` inside their own lock and fold
/// lookup keys exclusively with that lock-internal copy. Because `005 CASEMAPPING=...` re-keys
/// those three maps sequentially across separate locks, folding via `AtomicCaseMap::load()`
/// during the torn window would hash lookups under the new casemap before a map's keys were
/// re-hashed.
#[derive(Clone, Debug)]
pub(crate) struct AtomicCaseMap(Arc<AtomicU8>);

impl Default for AtomicCaseMap {
    fn default() -> Self {
        Self::new(CaseMap::default())
    }
}

impl AtomicCaseMap {
    pub(crate) fn new(initial: CaseMap) -> Self {
        Self(Arc::new(AtomicU8::new(initial.to_u8())))
    }

    #[inline]
    pub(crate) fn load(&self) -> CaseMap {
        CaseMap::from_u8(self.0.load(Ordering::Relaxed))
    }

    #[inline]
    pub(crate) fn store(&self, cm: CaseMap) {
        self.0.store(cm.to_u8(), Ordering::Relaxed);
    }
}

/// Canonical case-folded IRC identifier (`Ord + Eq + Hash`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FoldedStr(String);

impl FoldedStr {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for FoldedStr {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FoldedStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
#[path = "casemap_test.rs"]
mod test;
