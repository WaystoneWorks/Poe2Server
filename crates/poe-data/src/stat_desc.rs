//! Stat description formatter: `Data/StatDescriptions/*.csd`, the game's stat text templates.
//!
//! A file is UTF-16 text made of `include "path"`, `no_description <stat>` and `description`
//! blocks. A block names its stats, then lists variants: one range per stat (`#`, `N`, `A|B`,
//! `!N`), the quoted text, and value transforms (`negate 1`, `per_minute_to_per_second 1`, ...).
//! The first variant whose ranges all match is the wording; translated `lang` sections are skipped.
//! Text uses `{0}`, `{}`, `{0:+d}` placeholders and `[Term|Shown text]` links.

use std::collections::{HashMap, HashSet};

use crate::{GameFiles, Result};

/// The value range of one stat in a variant.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Range {
    Any,
    Between(Option<i64>, Option<i64>),
    Not(i64),
}

impl Range {
    fn parse(token: &str) -> Option<Self> {
        if token == "#" {
            return Some(Self::Any);
        }
        if let Some(n) = token.strip_prefix('!') {
            return n.parse().ok().map(Self::Not);
        }
        if let Some((lo, hi)) = token.split_once('|') {
            let bound = |s: &str| if s == "#" { Ok(None) } else { s.parse().map(Some) };
            return Some(Self::Between(bound(lo).ok()?, bound(hi).ok()?));
        }
        let n = token.parse().ok()?;
        Some(Self::Between(Some(n), Some(n)))
    }

    fn matches(self, v: i64) -> bool {
        match self {
            Self::Any => true,
            Self::Not(n) => v != n,
            Self::Between(lo, hi) => lo.is_none_or(|l| v >= l) && hi.is_none_or(|h| v <= h),
        }
    }
}

#[derive(Debug, Clone)]
struct Variant {
    ranges: Vec<Range>,
    text: String,
    /// `(transform name, stat index)`.
    transforms: Vec<(String, usize)>,
}

#[derive(Debug, Clone)]
struct Description {
    stats: Vec<String>,
    variants: Vec<Variant>,
}

/// A formatted line and the stats it used.
#[derive(Debug, Clone, PartialEq)]
pub struct StatLine {
    pub stats: Vec<String>,
    pub text: String,
}

/// One parsed description file with its includes.
#[derive(Debug, Default, Clone)]
pub struct StatDescriptions {
    descriptions: Vec<Description>,
    by_stat: HashMap<String, usize>,
    hidden: HashSet<String>,
}

impl StatDescriptions {
    /// Loads `path` (e.g. `data/statdescriptions/stat_descriptions.csd`) and what it includes.
    pub fn load(files: &GameFiles, path: &str) -> Result<Self> {
        let mut out = Self::default();
        out.load_file(files, &path.to_ascii_lowercase())?;
        Ok(out)
    }

    fn load_file(&mut self, files: &GameFiles, path: &str) -> Result<()> {
        let bytes = files.read(path)?;
        let words: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let text = String::from_utf16_lossy(&words);
        for inc in includes(&text) {
            self.load_file(files, &inc.to_ascii_lowercase())?;
        }
        self.add_text(&text);
        Ok(())
    }

    /// Parses file text that has no includes (the includes are the caller's job).
    pub fn parse(text: &str) -> Self {
        let mut out = Self::default();
        out.add_text(text);
        out
    }

    fn add_text(&mut self, text: &str) {
        let mut lines = text.lines().map(|l| l.trim_start_matches('\u{feff}')).peekable();
        while let Some(line) = lines.next() {
            let t = line.trim();
            if let Some(stat) = t.strip_prefix("no_description ") {
                self.hidden.insert(stat.trim().to_owned());
            } else if t == "description" || t.starts_with("description ") {
                if let Some(d) = parse_description(&mut lines) {
                    let idx = self.descriptions.len();
                    for s in &d.stats {
                        // The including file comes later and wins over what it includes.
                        self.by_stat.insert(s.clone(), idx);
                        self.hidden.remove(s);
                    }
                    self.descriptions.push(d);
                }
            }
        }
    }

    /// Adds another file's stats; the descriptions already here keep priority.
    pub fn merge(&mut self, other: Self) {
        let base = self.descriptions.len();
        self.descriptions.extend(other.descriptions);
        for (stat, idx) in other.by_stat {
            self.by_stat.entry(stat).or_insert(base + idx);
        }
        for stat in other.hidden {
            if !self.by_stat.contains_key(&stat) {
                self.hidden.insert(stat);
            }
        }
    }

    pub fn stat_count(&self) -> usize {
        self.by_stat.len()
    }

    /// Whether the file has a description for this stat.
    pub fn has(&self, stat: &str) -> bool {
        self.by_stat.contains_key(stat)
    }

    /// Whether the file says this stat is never shown.
    pub fn is_hidden(&self, stat: &str) -> bool {
        self.hidden.contains(stat)
    }

    /// The stats sharing a description with `stat`, in the description's order.
    pub fn group(&self, stat: &str) -> Option<&[String]> {
        self.by_stat
            .get(stat)
            .map(|&i| self.descriptions[i].stats.as_slice())
    }

    /// Formats stats as the client does: stats of one description share a line, a missing stat of
    /// the group counts as 0, a value set no variant matches shows nothing.
    pub fn format(&self, stats: &[(&str, i64)]) -> Vec<StatLine> {
        let values: HashMap<&str, i64> = stats.iter().copied().collect();
        let mut done = HashSet::new();
        let mut out = Vec::new();
        for (stat, _) in stats {
            let Some(&idx) = self.by_stat.get(*stat) else {
                continue;
            };
            if !done.insert(idx) {
                continue;
            }
            let d = &self.descriptions[idx];
            let vals: Vec<i64> = d
                .stats
                .iter()
                .map(|s| values.get(s.as_str()).copied().unwrap_or(0))
                .collect();
            if let Some(v) = d.variants.iter().find(|v| {
                v.ranges.len() == vals.len()
                    && v.ranges.iter().zip(&vals).all(|(r, x)| r.matches(*x))
            }) {
                out.push(StatLine {
                    stats: d.stats.clone(),
                    text: render(v, &vals),
                });
            }
        }
        out
    }
}

fn includes(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim_start_matches('\u{feff}').trim();
            let rest = l.strip_prefix("include ")?;
            Some(rest.trim().trim_matches('"').to_owned())
        })
        .collect()
}

fn parse_description<'a>(
    lines: &mut std::iter::Peekable<impl Iterator<Item = &'a str>>,
) -> Option<Description> {
    let header = lines.next()?;
    let mut parts = header.split_whitespace();
    let n: usize = parts.next()?.parse().ok()?;
    let stats: Vec<String> = parts.map(str::to_owned).collect();
    if stats.len() != n {
        return None;
    }
    let count: usize = lines.next()?.trim().parse().ok()?;
    let mut variants = Vec::with_capacity(count);
    for _ in 0..count {
        if let Some(v) = parse_variant(lines.next()?, n) {
            variants.push(v);
        }
    }
    // The translations follow up to the next block; `add_text` ignores them line by line, but a
    // translated variant line never starts a block, so nothing else is needed here.
    Some(Description { stats, variants })
}

fn parse_variant(line: &str, stats: usize) -> Option<Variant> {
    let open = line.find('"')?;
    let bytes = line.as_bytes();
    let mut close = open + 1;
    while close < bytes.len() && (bytes[close] != b'"' || bytes[close - 1] == b'\\') {
        close += 1;
    }
    let ranges: Vec<Range> = line[..open]
        .split_whitespace()
        .take(stats)
        .map(Range::parse)
        .collect::<Option<_>>()?;
    let text = line.get(open + 1..close)?.to_owned();
    let mut transforms = Vec::new();
    let mut rest = line.get(close + 1..).unwrap_or("").split_whitespace().peekable();
    while let Some(name) = rest.next() {
        // Flags such as `canonical_line` take no index; the others name a stat (1-based).
        let index = rest.peek().and_then(|t| t.parse::<usize>().ok());
        if index.is_some() {
            rest.next();
        }
        if let Some(i) = index {
            transforms.push((name.to_owned(), i.saturating_sub(1)));
        }
    }
    Some(Variant { ranges, text, transforms })
}

fn apply_transform(name: &str, v: f64) -> Option<(f64, Option<usize>)> {
    Some(match name {
        "negate" => (-v, None),
        "negate_and_double" => (-v * 2.0, None),
        "double" => (v * 2.0, None),
        "times_twenty" => (v * 20.0, None),
        "add_one" => (v + 1.0, None),
        "subtract_one" => (v - 1.0, None),
        "plus_two_hundred" => (v + 200.0, None),
        "divide_by_three" => (v / 3.0, Some(2)),
        "divide_by_four" => (v / 4.0, Some(2)),
        "divide_by_five" => (v / 5.0, Some(2)),
        "divide_by_fifty" => (v / 50.0, Some(2)),
        "divide_by_one_hundred" => (v / 100.0, Some(2)),
        "divide_by_one_hundred_and_negate" => (-v / 100.0, Some(2)),
        "divide_by_one_hundred_2dp" | "divide_by_one_hundred_2dp_if_required" => (v / 100.0, Some(2)),
        "per_minute_to_per_second" => (v / 60.0, Some(1)),
        "per_minute_to_per_second_0dp" => (v / 60.0, Some(0)),
        "per_minute_to_per_second_1dp" => (v / 60.0, Some(1)),
        "per_minute_to_per_second_2dp" | "per_minute_to_per_second_2dp_if_required" => (v / 60.0, Some(2)),
        "milliseconds_to_seconds" | "milliseconds_to_seconds_2dp_if_required" | "milliseconds_to_seconds_2dp" => {
            (v / 1000.0, Some(2))
        }
        "milliseconds_to_seconds_0dp" => (v / 1000.0, Some(0)),
        "milliseconds_to_seconds_1dp" => (v / 1000.0, Some(1)),
        "deciseconds_to_seconds" => (v / 10.0, Some(1)),
        _ => return None,
    })
}

/// A number as the client prints it: at most `dp` decimals, trailing zeros removed.
fn number(v: f64, dp: usize) -> String {
    let s = format!("{v:.dp$}");
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        s
    }
}

fn render(variant: &Variant, vals: &[i64]) -> String {
    let mut shown: Vec<(f64, usize)> = vals.iter().map(|&v| (v as f64, 0)).collect();
    for (name, i) in &variant.transforms {
        if let Some(slot) = shown.get_mut(*i) {
            if let Some((v, dp)) = apply_transform(name, slot.0) {
                slot.0 = v;
                if let Some(dp) = dp {
                    slot.1 = dp;
                }
            }
        }
    }
    let mut out = String::new();
    let mut next = 0;
    let t = variant.text.replace("\\n", "\n");
    let mut rest = t.as_str();
    while let Some(start) = rest.find(['{', '[']) {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let close = if tail.starts_with('{') { '}' } else { ']' };
        let Some(end) = tail.find(close) else {
            out.push_str(tail);
            rest = "";
            break;
        };
        let inner = &tail[1..end];
        if close == ']' {
            // `[Term|Shown text]` or `[Term]`.
            out.push_str(inner.rsplit('|').next().unwrap_or(inner));
        } else {
            let (index, spec) = inner.split_once(':').unwrap_or((inner, ""));
            let i = if index.is_empty() {
                next += 1;
                next - 1
            } else {
                index.parse().unwrap_or(0)
            };
            let (v, dp) = shown.get(i).copied().unwrap_or((0.0, 0));
            if spec == "+d" && v >= 0.0 {
                out.push('+');
            }
            out.push_str(&number(v, dp));
        }
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "no_description level\n\
description\n\t1 base_maximum_life\n\t1\n\t\t# \"{0:+d} to maximum Life\"\n\
\tlang \"German\"\n\t1\n\t\t# \"{0:+d} zu Leben\"\n\
description\n\t1 life_regeneration_rate_per_minute_%\n\t1\n\t\t# \"Regenerate {0}% of maximum Life per second\" per_minute_to_per_second 1\n\
description\n\t1 maximum_life_+%\n\t2\n\t\t1|# \"{0}% increased maximum [Life|Life]\"\n\t\t#|-1 \"{0}% reduced maximum Life\" negate 1\n\
description\n\t2 local_minimum_added_fire_damage local_maximum_added_fire_damage\n\t1\n\t\t# # \"Adds {0} to {1} [Fire|Fire] Damage\" canonical_line\n\
description\n\t1 base_skill_area\n\t2\n\t\t!0 \"{}% [AreaOfEffect|Area]\"\n\t\t0 \"none\"\n";

    fn line(d: &StatDescriptions, stats: &[(&str, i64)]) -> Vec<String> {
        d.format(stats).into_iter().map(|l| l.text).collect()
    }

    #[test]
    fn signed_and_translation_skip() {
        let d = StatDescriptions::parse(FILE);
        assert_eq!(line(&d, &[("base_maximum_life", 40)]), ["+40 to maximum Life"]);
        assert_eq!(line(&d, &[("base_maximum_life", -4)]), ["-4 to maximum Life"]);
    }

    #[test]
    fn ranges_negate_and_links() {
        let d = StatDescriptions::parse(FILE);
        assert_eq!(line(&d, &[("maximum_life_+%", 12)]), ["12% increased maximum Life"]);
        assert_eq!(line(&d, &[("maximum_life_+%", -7)]), ["7% reduced maximum Life"]);
        // No variant matches 0.
        assert!(line(&d, &[("maximum_life_+%", 0)]).is_empty());
        assert_eq!(line(&d, &[("base_skill_area", 5)]), ["5% Area"]);
    }

    #[test]
    fn per_minute_keeps_one_decimal() {
        let d = StatDescriptions::parse(FILE);
        assert_eq!(
            line(&d, &[("life_regeneration_rate_per_minute_%", 90)]),
            ["Regenerate 1.5% of maximum Life per second"]
        );
    }

    #[test]
    fn grouped_stats_share_a_line() {
        let d = StatDescriptions::parse(FILE);
        let got = d.format(&[
            ("local_maximum_added_fire_damage", 9),
            ("local_minimum_added_fire_damage", 3),
        ]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "Adds 3 to 9 Fire Damage");
    }

    #[test]
    fn hidden_and_unknown() {
        let d = StatDescriptions::parse(FILE);
        assert!(d.is_hidden("level"));
        assert!(!d.has("level"));
        assert!(line(&d, &[("not_a_stat", 1)]).is_empty());
    }
}
