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
#[path = "casemap_test.rs"]
mod test;
