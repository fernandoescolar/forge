//! NuGet versions: SemVer 2 with up to four numeric parts, compared the way NuGet does
//! (release labels compare part by part, numeric parts numerically, case-insensitively;
//! build metadata is ignored).

use std::cmp::Ordering;

#[derive(Clone, Debug, Eq)]
pub struct NuGetVersion {
    pub parts: [u64; 4],
    pub release: Vec<String>,
    pub original: String,
}

impl NuGetVersion {
    pub fn parse(text: &str) -> Option<Self> {
        let original = text.trim().to_string();
        let without_metadata = original.split('+').next()?;
        let (numbers, release) = match without_metadata.split_once('-') {
            Some((numbers, release)) => (numbers, release.split('.').map(str::to_string).collect()),
            None => (without_metadata, Vec::new()),
        };
        let mut parts = [0u64; 4];
        let mut count = 0;
        for (i, part) in numbers.split('.').enumerate() {
            if i >= 4 {
                return None;
            }
            parts[i] = part.parse().ok()?;
            count += 1;
        }
        (count > 0).then_some(Self { parts, release, original })
    }

    pub fn is_prerelease(&self) -> bool {
        !self.release.is_empty()
    }

    /// `1.0.0` for `1.0`, `1.0.0.0`, …; keeps the fourth part when it is not zero.
    pub fn normalized(&self) -> String {
        let [a, b, c, d] = self.parts;
        let mut text = if d != 0 { format!("{a}.{b}.{c}.{d}") } else { format!("{a}.{b}.{c}") };
        if self.is_prerelease() {
            text.push('-');
            text.push_str(&self.release.join("."));
        }
        text
    }
}

impl PartialEq for NuGetVersion {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl PartialOrd for NuGetVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for NuGetVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        self.parts.cmp(&other.parts).then_with(|| match (self.release.is_empty(), other.release.is_empty()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => {
                for (a, b) in self.release.iter().zip(&other.release) {
                    let ordering = match (a.parse::<u64>(), b.parse::<u64>()) {
                        (Ok(x), Ok(y)) => x.cmp(&y),
                        (Ok(_), Err(_)) => Ordering::Less,
                        (Err(_), Ok(_)) => Ordering::Greater,
                        _ => a.to_lowercase().cmp(&b.to_lowercase()),
                    };
                    if ordering != Ordering::Equal {
                        return ordering;
                    }
                }
                self.release.len().cmp(&other.release.len())
            }
        })
    }
}

/// Compares two version strings; unparsable ones sort first.
pub fn compare(a: &str, b: &str) -> Ordering {
    match (NuGetVersion::parse(a), NuGetVersion::parse(b)) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => a.cmp(b),
    }
}

/// The lowest version a version or range admits: `1.0`, `[1.0, 2.0)`, `[1.0]`, `(, 2.0]`.
pub fn range_minimum(range: &str) -> Option<String> {
    let trimmed = range.trim();
    if !trimmed.starts_with(['[', '(']) {
        return (!trimmed.is_empty() && !trimmed.contains('*')).then(|| trimmed.to_string());
    }
    let inner = trimmed.trim_start_matches(['[', '(']).trim_end_matches([']', ')']);
    let min = inner.split(',').next()?.trim();
    (!min.is_empty()).then(|| min.to_string())
}

/// Versions sorted newest first, without prereleases unless asked (or unless there is
/// nothing else).
pub fn sort_desc(versions: impl IntoIterator<Item = String>, include_prerelease: bool) -> Vec<String> {
    let mut parsed: Vec<NuGetVersion> = versions.into_iter().filter_map(|v| NuGetVersion::parse(&v)).collect();
    parsed.sort_by(|a, b| b.cmp(a));
    parsed.dedup();
    let stable: Vec<String> = parsed.iter().filter(|v| !v.is_prerelease()).map(|v| v.original.clone()).collect();
    if include_prerelease || stable.is_empty() { parsed.into_iter().map(|v| v.original).collect() } else { stable }
}

/// The newest version, honoring the prerelease choice.
pub fn latest(versions: &[String], include_prerelease: bool) -> Option<String> {
    sort_desc(versions.iter().cloned(), include_prerelease).into_iter().next()
}

/// Whether `candidate` is newer than `current`.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    compare(candidate, current) == Ordering::Greater
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_like_nuget() {
        let mut versions = vec!["1.0.0", "1.0.0-beta.10", "1.0.0-beta.2", "1.0.0-alpha", "0.9", "1.0.0.1", "1.0.0-RC.1"];
        versions.sort_by(|a, b| compare(a, b));
        assert_eq!(versions, vec!["0.9", "1.0.0-alpha", "1.0.0-beta.2", "1.0.0-beta.10", "1.0.0-RC.1", "1.0.0", "1.0.0.1"]);
        assert_eq!(NuGetVersion::parse("1.0").unwrap(), NuGetVersion::parse("1.0.0.0").unwrap());
        assert_eq!(NuGetVersion::parse("1.2.0+abc").unwrap().normalized(), "1.2.0");
        assert!(NuGetVersion::parse("1.x").is_none());
    }

    #[test]
    fn ranges_and_latest() {
        assert_eq!(range_minimum("[1.2.3, )").as_deref(), Some("1.2.3"));
        assert_eq!(range_minimum("[1.0]").as_deref(), Some("1.0"));
        assert_eq!(range_minimum("(, 2.0]"), None);
        assert_eq!(range_minimum("4.0.1").as_deref(), Some("4.0.1"));
        let versions: Vec<String> = ["1.0.0", "2.0.0-preview.1", "1.5.0"].map(String::from).to_vec();
        assert_eq!(latest(&versions, false).as_deref(), Some("1.5.0"));
        assert_eq!(latest(&versions, true).as_deref(), Some("2.0.0-preview.1"));
        assert!(is_newer("1.10.0", "1.9.0"));
    }
}
