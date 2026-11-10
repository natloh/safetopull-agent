use std::fmt;

/// A release version of up to four numeric parts (`v4.0.11.2680`). Missing parts are zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version([u64; 4]);

impl Version {
    pub fn from_parts(parts: &[u64]) -> Self {
        let mut padded = [0; 4];
        for (slot, part) in padded.iter_mut().zip(parts) {
            *slot = *part;
        }
        Self(padded)
    }

    /// Parses release and image tags such as `v1.132.0`, `n8n@1.2.3`, `2.0.0-lts` or `1.2.3-ls45`.
    /// Returns `None` for rolling tags (`latest`, `release`) and anything not led by a number.
    pub fn parse(tag: &str) -> Option<Self> {
        let tag = tag.trim();
        let tag = tag.rsplit_once('@').map_or(tag, |(_, version)| version);
        let tag = tag.trim_start_matches(['v', 'V']);

        let numeric_len = tag
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(tag.len());
        let (numeric, suffix) = tag.split_at(numeric_len);

        if !(suffix.is_empty() || suffix.starts_with(['-', '+'])) {
            return None;
        }

        let parts = numeric
            .split('.')
            .map(|part| part.parse::<u64>().ok())
            .collect::<Option<Vec<_>>>()?;

        (1..=4).contains(&parts.len()).then(|| Self::from_parts(&parts))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [major, minor, patch, build] = self.0;
        write!(f, "{major}.{minor}.{patch}")?;
        if build > 0 {
            write!(f, ".{build}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Version;

    #[test]
    fn parses_the_tag_shapes_the_dataset_tracks() {
        let cases = [
            ("v1.132.0", [1, 132, 0, 0]),
            ("1.132", [1, 132, 0, 0]),
            ("16", [16, 0, 0, 0]),
            ("v4.0.11.2680", [4, 0, 11, 2680]),
            ("n8n@1.70.2", [1, 70, 2, 0]),
            ("2.426.1-lts", [2, 426, 1, 0]),
            ("1.2.3-ls45", [1, 2, 3, 0]),
        ];

        for (tag, parts) in cases {
            assert_eq!(Version::parse(tag), Some(Version::from_parts(&parts)), "{tag}");
        }
    }

    #[test]
    fn rejects_rolling_and_malformed_tags() {
        for tag in ["latest", "release", "", "v", "1..2", "1.2.3.4.5", "1.2.3abc", "pg14-v0.2.0"] {
            assert_eq!(Version::parse(tag), None, "{tag}");
        }
    }

    #[test]
    fn orders_by_all_four_parts() {
        let parse = |tag| Version::parse(tag).unwrap();

        assert!(parse("v1.133.0") > parse("v1.132.9"));
        assert!(parse("v4.0.11.2680") > parse("v4.0.11"));
        assert_eq!(parse("v2.0"), parse("2.0.0"));
    }
}
