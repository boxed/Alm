//! Reading the local package cache (`~/.elm/0.19.1/packages`).
//!
//! alm never goes to the network: everything it knows about a package comes
//! from what `elm` has already downloaded. That is enough to resolve a
//! dependency set — each cached package carries its own `elm.json` with its
//! constraints — and it keeps `alm init`/`install` usable offline, at the cost
//! of only ever choosing among versions already on the machine.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// A package version, ordered the way Elm orders them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    pub fn parse(text: &str) -> Option<Version> {
        let mut parts = text.trim().split('.');
        let mut next = || parts.next()?.parse().ok();
        let version = Version { major: next()?, minor: next()?, patch: next()? };
        parts.next().is_none().then_some(version)
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// An Elm version constraint: `1.0.0 <= v < 2.0.0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Constraint {
    pub low: Version,
    pub low_inclusive: bool,
    pub high: Version,
    pub high_inclusive: bool,
}

impl Constraint {
    /// Parse `<low> <op> v <op> <high>`, the only shape Elm allows.
    pub fn parse(text: &str) -> Option<Constraint> {
        let parts: Vec<&str> = text.split_whitespace().collect();
        let [low, low_op, "v", high_op, high] = parts[..] else {
            return None;
        };
        Some(Constraint {
            low: Version::parse(low)?,
            low_inclusive: low_op == "<=",
            high: Version::parse(high)?,
            high_inclusive: high_op == "<=",
        })
    }

    /// Everything, for a dependency named with no opinion about the version.
    pub fn anything() -> Constraint {
        Constraint {
            low: Version { major: 1, minor: 0, patch: 0 },
            low_inclusive: true,
            high: Version { major: u32::MAX, minor: 0, patch: 0 },
            high_inclusive: false,
        }
    }

    pub fn allows(&self, version: Version) -> bool {
        let above = if self.low_inclusive { version >= self.low } else { version > self.low };
        let below = if self.high_inclusive { version <= self.high } else { version < self.high };
        above && below
    }

    /// The constraint elm writes for a chosen version: `1.2.3 <= v < 2.0.0`.
    pub fn covering(version: Version) -> Constraint {
        Constraint {
            low: version,
            low_inclusive: true,
            high: Version { major: version.major + 1, minor: 0, patch: 0 },
            high_inclusive: false,
        }
    }
}

impl std::fmt::Display for Constraint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let low_op = if self.low_inclusive { "<=" } else { "<" };
        let high_op = if self.high_inclusive { "<=" } else { "<" };
        write!(f, "{} {} v {} {}", self.low, low_op, high_op, self.high)
    }
}

/// The cache directory holding `<author>/<name>/<version>`.
pub fn packages_root() -> PathBuf {
    let home = std::env::var("ELM_HOME").map(PathBuf::from).unwrap_or_else(|_| {
        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".elm")
    });
    home.join("0.19.1").join("packages")
}

/// Every version of `author/name` on this machine, ascending. A directory
/// without an `elm.json` is a partial download and is skipped.
pub fn cached_versions(package: &str) -> Vec<Version> {
    let Some((author, name)) = package.split_once('/') else {
        return Vec::new();
    };
    let dir = packages_root().join(author).join(name);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut versions: Vec<Version> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            entry.path().join("elm.json").is_file().then_some(())?;
            Version::parse(entry.file_name().to_str()?)
        })
        .collect();
    versions.sort();
    versions
}

/// A cached package's own `elm.json`.
pub fn read_manifest(package: &str, version: Version) -> Option<String> {
    let (author, name) = package.split_once('/')?;
    let path = packages_root().join(author).join(name).join(version.to_string()).join("elm.json");
    std::fs::read_to_string(path).ok()
}

/// The `"dependencies"` a cached package declares: name to constraint.
pub fn dependencies_of(package: &str, version: Version) -> BTreeMap<String, Constraint> {
    let Some(manifest) = read_manifest(package, version) else {
        return BTreeMap::new();
    };
    let Some(block) = object_block(&manifest, "dependencies") else {
        return BTreeMap::new();
    };
    pairs(block)
        .into_iter()
        .filter_map(|(name, value)| Some((name.to_string(), Constraint::parse(value)?)))
        .collect()
}

/// Why a dependency set could not be resolved.
#[derive(Debug)]
pub enum SolveError {
    /// Nothing satisfying the constraints is in the cache.
    Unsatisfiable(String),
}

/// Resolve `roots` to concrete versions, together with everything they need.
///
/// A straightforward highest-version-first search over the cache: take the
/// newest cached version allowed by the constraints gathered so far, and when
/// a later dependency narrows a package already chosen, re-pick and start
/// again. Elm's own solver does the same thing against the registry; the only
/// difference is the set of versions to choose from.
pub fn solve(
    roots: &BTreeMap<String, Constraint>,
) -> Result<BTreeMap<String, Version>, SolveError> {
    // Constraints accumulated per package, tightened as dependencies are seen.
    let mut wanted: BTreeMap<String, Vec<Constraint>> =
        roots.iter().map(|(name, c)| (name.clone(), vec![*c])).collect();

    // Bounded so a pathological cache cannot spin forever; each pass either
    // settles or tightens at least one package.
    for _ in 0..64 {
        let mut chosen: BTreeMap<String, Version> = BTreeMap::new();
        let mut queue: Vec<String> = wanted.keys().cloned().collect();
        let mut restart = false;

        while let Some(package) = queue.pop() {
            if chosen.contains_key(&package) {
                continue;
            }
            let constraints = wanted.get(&package).cloned().unwrap_or_default();
            let best = cached_versions(&package)
                .into_iter()
                .filter(|v| constraints.iter().all(|c| c.allows(*v)))
                .next_back();
            let Some(version) = best else {
                return Err(SolveError::Unsatisfiable(package));
            };
            chosen.insert(package.clone(), version);
            for (dependency, constraint) in dependencies_of(&package, version) {
                let entry = wanted.entry(dependency.clone()).or_default();
                if !entry.contains(&constraint) {
                    entry.push(constraint);
                    // A package already picked may no longer be allowed.
                    if chosen.get(&dependency).is_some_and(|v| !constraint.allows(*v)) {
                        restart = true;
                    }
                }
                queue.push(dependency);
            }
        }

        if !restart {
            return Ok(chosen);
        }
    }
    Err(SolveError::Unsatisfiable("(constraints do not settle)".to_string()))
}

/// The string value of a top-level `"key": "value"`.
pub fn json_string<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let at = json.find(&format!("\"{key}\""))?;
    let rest = &json[at + key.len() + 2..];
    let colon = rest.find(':')?;
    let after = &rest[colon + 1..];
    let open = after.find('"')?;
    let value = &after[open + 1..];
    let end = value.find('"')?;
    Some(&value[..end])
}

/// Replace the body of the `{ … }` following `"key":`, keeping the rest of the
/// document byte-for-byte. An elm.json may carry fields alm does not model, so
/// rewriting only the block that changed is safer than regenerating the file.
pub fn replace_object_block(json: &str, key: &str, body: &str) -> Option<String> {
    let at = json.find(&format!("\"{key}\""))?;
    let rest = &json[at..];
    let open = rest.find('{')?;
    let mut depth = 0;
    for (i, byte) in rest.as_bytes()[open..].iter().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    let start = at + open + 1;
                    let end = at + open + i;
                    return Some(format!("{}{}{}", &json[..start], body, &json[end..]));
                }
            }
            _ => {}
        }
    }
    None
}

/// The `{ … }` following `"key":`, without its braces.
pub fn object_block<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let at = json.find(&format!("\"{key}\""))?;
    let rest = &json[at..];
    let open = rest.find('{')?;
    let mut depth = 0;
    for (i, byte) in rest.as_bytes()[open..].iter().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&rest[open + 1..open + i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// The `"key": "value"` pairs directly inside an object body.
pub fn pairs(block: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let mut rest = block;
    while let Some(start) = rest.find('"') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('"') else { break };
        let key = &after[..end];
        let tail = &after[end + 1..];
        let Some(colon) = tail.find(':') else { break };
        let value_part = &tail[colon + 1..];
        let Some(vstart) = value_part.find('"') else { break };
        let value_rest = &value_part[vstart + 1..];
        let Some(vend) = value_rest.find('"') else { break };
        out.push((key, &value_rest[..vend]));
        rest = &value_rest[vend + 1..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_order_numerically_not_lexically() {
        let mut vs = vec![
            Version::parse("1.0.10").unwrap(),
            Version::parse("1.0.2").unwrap(),
            Version::parse("1.1.0").unwrap(),
        ];
        vs.sort();
        assert_eq!(
            vs.iter().map(|v| v.to_string()).collect::<Vec<_>>(),
            vec!["1.0.2", "1.0.10", "1.1.0"]
        );
    }

    #[test]
    fn constraints_honor_their_bounds() {
        let c = Constraint::parse("1.0.0 <= v < 2.0.0").unwrap();
        assert!(c.allows(Version::parse("1.0.0").unwrap()));
        assert!(c.allows(Version::parse("1.9.9").unwrap()));
        assert!(!c.allows(Version::parse("2.0.0").unwrap()));
        assert!(!c.allows(Version::parse("0.9.9").unwrap()));
        assert_eq!(c.to_string(), "1.0.0 <= v < 2.0.0");
    }

    #[test]
    fn a_chosen_version_is_covered_up_to_the_next_major() {
        let c = Constraint::covering(Version::parse("1.2.3").unwrap());
        assert_eq!(c.to_string(), "1.2.3 <= v < 2.0.0");
    }

    #[test]
    fn malformed_constraints_are_rejected_rather_than_guessed() {
        assert!(Constraint::parse("1.0.0").is_none());
        assert!(Constraint::parse("1.0.0 <= x < 2.0.0").is_none());
        assert!(Version::parse("1.0").is_none());
        assert!(Version::parse("1.0.0.0").is_none());
    }
}
