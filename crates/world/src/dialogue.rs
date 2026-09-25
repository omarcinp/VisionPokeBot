//! Game text (`data/world/dialogue.json`): every text label as the pages
//! and lines the game prints, and identification of on-screen lines.

use std::collections::BTreeMap;
use std::path::Path;

use pokebot_core::Result;
use serde::Deserialize;

use crate::events::load_json;

#[derive(Debug, Clone, Deserialize)]
pub struct Dialogue {
    pub rom: String,
    #[serde(default)]
    pub sha1: String,
    /// Label → pages → lines. Names and numbers the game fills in are `*`.
    pub labels: BTreeMap<String, Vec<Vec<String>>>,
    /// First line → labels whose text starts with it.
    pub index: BTreeMap<String, Vec<String>>,
}

impl Dialogue {
    /// Loads `dir/dialogue.json`.
    pub fn load(dir: impl AsRef<Path>) -> Result<Dialogue> {
        load_json(&dir.as_ref().join("dialogue.json"))
    }

    /// Labels whose text starts with `lines` as read off the screen. `*` in
    /// the label's text matches any run of characters (a name, a number);
    /// `?` in a read line matches any single character (OCR doubt). Every
    /// line given must match the label's next line; later lines narrow the
    /// candidates. Sorted, for determinism.
    pub fn identify(&self, lines: &[String]) -> Vec<&str> {
        let Some(first) = lines.first() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (pattern, labels) in &self.index {
            if !glob_match(pattern, first) {
                continue;
            }
            for label in labels {
                let text: Vec<&String> = self.labels[label].iter().flatten().collect();
                let rest_matches = lines
                    .iter()
                    .enumerate()
                    .skip(1)
                    .all(|(i, line)| text.get(i).is_some_and(|t| glob_match(t, line)));
                if rest_matches {
                    out.push(label.as_str());
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }
}

/// `pattern` with `*` wildcards against `text` with `?` unknowns.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.trim().chars().collect();
    let t: Vec<char> = text.trim().chars().collect();
    glob(&p, &t)
}

fn glob(p: &[char], t: &[char]) -> bool {
    match p.split_first() {
        None => t.is_empty(),
        Some(('*', rest)) => {
            // Collapse runs of stars, then try every split.
            let rest = rest
                .iter()
                .position(|c| *c != '*')
                .map_or(&rest[rest.len()..], |i| &rest[i..]);
            (0..=t.len()).any(|i| glob(rest, &t[i..]))
        }
        Some((c, rest)) => t
            .split_first()
            .is_some_and(|(d, t_rest)| (*d == '?' || fold(*d) == fold(*c)) && glob(rest, t_rest)),
    }
}

/// The game prints the script's ASCII quotes as its curly glyphs (the
/// charmap's `'` is `’`): the two read alike.
fn fold(c: char) -> char {
    match c {
        '’' | '‘' => '\'',
        '“' | '”' => '"',
        c => c,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcards_and_unknowns() {
        assert!(glob_match("*'s house", "RED's house"));
        assert!(glob_match("*'s house", "'s house"));
        assert!(!glob_match("*'s house", "RED's houses"));
        assert!(glob_match("* used *!", "PIKACHU used THUNDERBOLT!"));
        assert!(glob_match("down!", "d?wn!"));
        assert!(glob_match("Caught * POKéMON!", "Caught 15 POKé?ON!"));
        assert!(!glob_match("down!", "down"));
        assert!(glob_match("a**b", "axyzb"));
        // Seen on screen: the game's curly apostrophe for the script's `'`.
        assert!(glob_match("OAK: It's unsafe!", "OAK: It’s unsafe!"));
    }

    #[test]
    fn identify_narrows_by_later_lines() {
        let mut labels = BTreeMap::new();
        labels.insert(
            "A".to_string(),
            vec![vec!["Hello, *!".to_string(), "Bye.".to_string()]],
        );
        labels.insert(
            "B".to_string(),
            vec![vec!["Hello, *!".to_string()], vec!["Again.".to_string()]],
        );
        let mut index = BTreeMap::new();
        index.insert(
            "Hello, *!".to_string(),
            vec!["A".to_string(), "B".to_string()],
        );
        let d = Dialogue {
            rom: String::new(),
            sha1: String::new(),
            labels,
            index,
        };
        let l = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(d.identify(&l(&["Hello, RED!"])), vec!["A", "B"]);
        assert_eq!(d.identify(&l(&["Hello, RED!", "Bye."])), vec!["A"]);
        assert_eq!(d.identify(&l(&["Hello, RED!", "Again."])), vec!["B"]);
        assert!(d.identify(&l(&["Hello, RED!", "Bye.", "More"])).is_empty());
        assert!(d.identify(&l(&[])).is_empty());
    }
}
