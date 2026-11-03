//! Agent versions: parsing, ordering and the "tested" range each adapter declares.
//!
//! Versions look like `2.1.309` or `0.165.0-alpha.1.2`. Ordering follows semver: numeric parts
//! first, then a release sorts after any of its pre-releases, pre-release identifiers compare
//! numerically when both are numbers and as text otherwise.

use std::cmp::Ordering;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    /// The text as the agent wrote it.
    pub raw: String,
    core: Vec<u64>,
    pre: Vec<String>,
}

impl Version {
    /// `None` unless the text is digits separated by dots (1 to 4 parts), optionally followed
    /// by `-` and dot-separated pre-release identifiers.
    pub fn parse(text: &str) -> Option<Version> {
        let text = text.trim();
        let (core, pre) = match text.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (text, None),
        };
        let core: Vec<u64> = core.split('.').map(|p| p.parse().ok()).collect::<Option<_>>()?;
        if core.is_empty() || core.len() > 4 {
            return None;
        }
        let pre: Vec<String> = match pre {
            Some(p) => {
                let ids: Vec<String> = p.split('.').map(str::to_owned).collect();
                if ids.iter().any(|i| i.is_empty() || !i.chars().all(|c| c.is_ascii_alphanumeric())) {
                    return None;
                }
                ids
            }
            None => Vec::new(),
        };
        Some(Version { raw: text.to_owned(), core, pre })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        let n = self.core.len().max(other.core.len());
        for i in 0..n {
            let (a, b) = (self.core.get(i).copied().unwrap_or(0), other.core.get(i).copied().unwrap_or(0));
            match a.cmp(&b) {
                Ordering::Equal => {}
                o => return o,
            }
        }
        match (self.pre.is_empty(), other.pre.is_empty()) {
            (true, true) => return Ordering::Equal,
            (true, false) => return Ordering::Greater,
            (false, true) => return Ordering::Less,
            (false, false) => {}
        }
        for (a, b) in self.pre.iter().zip(&other.pre) {
            let o = match (a.parse::<u64>(), b.parse::<u64>()) {
                (Ok(x), Ok(y)) => x.cmp(&y),
                (Ok(_), Err(_)) => Ordering::Less,
                (Err(_), Ok(_)) => Ordering::Greater,
                (Err(_), Err(_)) => a.cmp(b),
            };
            if o != Ordering::Equal {
                return o;
            }
        }
        self.pre.len().cmp(&other.pre.len())
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The versions an adapter was checked against, inclusive. Anything outside is "not
/// recognised": the adapter stops instead of guessing.
#[derive(Debug, Clone, Copy)]
pub struct Tested {
    pub min: &'static str,
    pub max: &'static str,
}

impl Tested {
    pub fn contains(&self, v: &Version) -> bool {
        match (Version::parse(self.min), Version::parse(self.max)) {
            (Some(lo), Some(hi)) => *v >= lo && *v <= hi,
            _ => false,
        }
    }

    /// `Ok(version)` when `text` parses and is inside the range.
    pub fn check(&self, text: &str) -> Result<Version, String> {
        match Version::parse(text) {
            Some(v) if self.contains(&v) => Ok(v),
            Some(v) => Err(format!("version {v} is not recognised (tested {} to {})", self.min, self.max)),
            None => Err(format!("version '{text}' is not recognised (not a version number)")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn parses_and_orders_like_semver() {
        assert!(v("2.1.309") > v("2.1.295"));
        assert!(v("2.1.10") > v("2.1.9"));
        assert!(v("0.165.0-alpha.1.2") > v("0.163.0-alpha.11.1"));
        assert!(v("0.165.0") > v("0.165.0-alpha.1.2"));
        assert!(v("0.165.0-alpha.2") > v("0.165.0-alpha.1.2"));
        assert!(v("1.0.0-alpha.beta") > v("1.0.0-alpha.1"));
        assert_eq!(v("2.1").cmp(&v("2.1.0")), Ordering::Equal);
        for bad in ["", "abc", "2..1", "2.1.x", "1.2.3.4.5", "1.0-", "1.0-al pha"] {
            assert!(Version::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn tested_range_is_inclusive_and_strict() {
        let t = Tested { min: "2.1.293", max: "2.1.309" };
        assert!(t.check("2.1.293").is_ok() && t.check("2.1.309").is_ok() && t.check("2.1.300").is_ok());
        assert!(t.check("2.1.310").unwrap_err().contains("not recognised"));
        assert!(t.check("2.1.292").is_err());
        assert!(t.check("garbage").is_err());
    }
}
