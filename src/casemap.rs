use std::fmt;

/// Server-negotiated IRC case-folding rule (`005 CASEMAPPING=rfc1459 | strict-rfc1459 | ascii`;
/// default `Rfc1459`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CaseMap {
    #[default]
    Rfc1459,
    StrictRfc1459,
    Ascii,
}

impl CaseMap {
    pub(crate) const fn to_u8(self) -> u8 {
        match self {
            CaseMap::Rfc1459 => 0,
            CaseMap::StrictRfc1459 => 1,
            CaseMap::Ascii => 2,
        }
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
        match self {
            CaseMap::Rfc1459 => match c {
                'A'..='Z' => (c as u8 + 32) as char,
                '[' => '{',
                ']' => '}',
                '\\' => '|',
                '~' => '^',
                _ => c,
            },
            CaseMap::StrictRfc1459 => match c {
                'A'..='Z' => (c as u8 + 32) as char,
                '[' => '{',
                ']' => '}',
                '\\' => '|',
                _ => c,
            },
            CaseMap::Ascii => c.to_ascii_lowercase(),
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
mod tests {
    use super::*;

    #[test]
    fn rfc1459_folding_and_eq() {
        let cm = CaseMap::Rfc1459;
        assert!(cm.str_eq("[Admin]", "{admin}"));
        assert_eq!(cm.fold("[Admin]"), cm.fold("{admin}"));
        assert!(cm.str_eq("~nick\\1", "^NICK|1"));
        assert_eq!(cm.fold("~nick\\1"), cm.fold("^NICK|1"));
        assert_eq!(cm.fold("[Alice]").as_str(), "{alice}");
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
    }

    #[test]
    fn isupport_parsing_and_u8_roundtrip() {
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
    }
}
