//! The mod database: PoB2's `ModStore`/`ModDB` (`Sum`, `More`, `Flag`, `Override`, tag evaluation).
//! Rounding follows PoB2: `More` multiplies the factors, then rounds the product to 2 decimals
//! (4 for the names in its high precision list).

use std::collections::HashMap;

use crate::round_to;

/// `ModFlag` values (Data/Global.lua). The bit values are PoB2's: the stat table uses them.
pub mod flag {
    pub const ATTACK: u64 = 0x1;
    pub const SPELL: u64 = 0x2;
    pub const HIT: u64 = 0x4;
    pub const CAST: u64 = 0x10;
    pub const MELEE: u64 = 0x100;
    pub const AREA: u64 = 0x200;
    pub const PROJECTILE: u64 = 0x400;
    pub const WEAPON: u64 = 0x2000;
    pub const AXE: u64 = 0x1_0000;
    pub const BOW: u64 = 0x2_0000;
    pub const CLAW: u64 = 0x4_0000;
    pub const DAGGER: u64 = 0x8_0000;
    pub const MACE: u64 = 0x10_0000;
    pub const STAFF: u64 = 0x20_0000;
    pub const SWORD: u64 = 0x40_0000;
    pub const WAND: u64 = 0x80_0000;
    pub const UNARMED: u64 = 0x100_0000;
    pub const CROSSBOW: u64 = 0x400_0000;
    pub const FLAIL: u64 = 0x800_0000;
    pub const SPEAR: u64 = 0x1000_0000;
    pub const WARSTAFF: u64 = 0x2000_0000;
    pub const TALISMAN: u64 = 0x4000_0000;
    pub const WEAPON_MELEE: u64 = 0x1_0000_0000;
    pub const WEAPON_RANGED: u64 = 0x2_0000_0000;
    pub const WEAPON_1H: u64 = 0x4_0000_0000;
    pub const WEAPON_2H: u64 = 0x8_0000_0000;
}

/// `KeywordFlag` values used by the default attacks.
pub mod keyword {
    pub const BOW: u32 = 0x400;
    pub const ARROW: u32 = 0x800;
    pub const ATTACK: u32 = 0x1_0000;
    pub const HIT: u32 = 0x4_0000;
    pub const MATCH_ALL: u32 = 0x4000_0000;
}

/// A mod's type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Base,
    Inc,
    More,
    Flag,
    Override,
    List,
}

/// A mod's condition or scaling.
#[derive(Debug, Clone, PartialEq)]
pub enum Tag {
    /// Applies only when the condition holds (`neg`: when it does not).
    Condition { var: &'static str, neg: bool },
    /// `value * floor(multiplier / div) + base`, multiplier capped by `limit`.
    Multiplier { var: &'static str, div: f64, limit: Option<f64>, base: f64 },
    /// `value * floor(stat / div) + base`, from an already computed stat.
    PerStat { stat: &'static str, div: f64, limit: Option<f64>, base: f64 },
    /// `ceil(value * stat * percent / 100)`.
    PercentStat { stat: &'static str, percent: f64 },
    /// Applies only to a skill of this type.
    SkillType(u32),
    /// Applies only to this item slot.
    SlotName(&'static str),
    /// Buffs: grouped by PoB2 before evaluation, it does not change the value (used from P16 on).
    GlobalEffect { effect: &'static str },
}

impl Tag {
    pub fn per_stat(stat: &'static str, div: f64) -> Self {
        Self::PerStat { stat, div, limit: None, base: 0.0 }
    }

    pub fn per_level(div: f64) -> Self {
        Self::Multiplier { var: "Level", div, limit: None, base: 0.0 }
    }

    pub fn condition(var: &'static str) -> Self {
        Self::Condition { var, neg: false }
    }
}

/// One modifier.
#[derive(Debug, Clone, PartialEq)]
pub struct Mod {
    pub name: String,
    pub kind: Kind,
    pub value: f64,
    pub flags: u64,
    pub keyword_flags: u32,
    pub tags: Vec<Tag>,
    /// Text value of a `List` mod.
    pub text: Option<String>,
}

impl Mod {
    pub fn new(name: impl Into<String>, kind: Kind, value: f64) -> Self {
        Self { name: name.into(), kind, value, flags: 0, keyword_flags: 0, tags: Vec::new(), text: None }
    }

    pub fn with_tag(mut self, tag: Tag) -> Self {
        self.tags.push(tag);
        self
    }

    pub fn with_flags(mut self, flags: u64) -> Self {
        self.flags = flags;
        self
    }
}

/// What is being asked about: PoB2's `cfg`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Cfg<'a> {
    pub flags: u64,
    pub keyword_flags: u32,
    pub slot: Option<&'a str>,
    pub skill_types: &'a [u32],
}

/// PoB2's `MatchKeywordFlags`.
fn keyword_match(query: u32, modk: u32) -> bool {
    let all = modk & keyword::MATCH_ALL != 0;
    let m = modk & !keyword::MATCH_ALL;
    let q = query & !keyword::MATCH_ALL;
    if all { q & m == m } else { m == 0 || q & m != 0 }
}

/// Mod names whose `More` keeps more than 2 decimals, with the decimals (`data.highPrecisionMods`).
const HIGH_PRECISION_MORE: &[(&str, i32)] = &[("SupportManaMultiplier", 4), ("ReservationMultiplier", 4)];

/// The mod database of one character.
#[derive(Debug, Default, Clone)]
pub struct ModDb {
    mods: HashMap<String, Vec<Mod>>,
    pub conditions: HashMap<&'static str, bool>,
    pub multipliers: HashMap<&'static str, f64>,
    /// Computed stats for `PerStat` tags (PoB2's actor output).
    pub stats: HashMap<&'static str, f64>,
}

impl ModDb {
    pub fn add(&mut self, m: Mod) {
        self.mods.entry(m.name.clone()).or_default().push(m);
    }

    pub fn new_mod(&mut self, name: &str, kind: Kind, value: f64) {
        self.add(Mod::new(name, kind, value));
    }

    pub fn condition(&self, var: &str) -> bool {
        self.conditions.get(var).copied().unwrap_or(false)
            || self.has_flag(&format!("Condition:{var}"))
    }

    fn has_flag(&self, name: &str) -> bool {
        self.mods.get(name).is_some_and(|l| l.iter().any(|m| m.kind == Kind::Flag && m.value != 0.0))
    }

    fn stat(&self, name: &str) -> f64 {
        self.stats.get(name).copied().unwrap_or(0.0)
    }

    /// PoB2's `EvalMod`: the mod's value after its tags, `None` when a tag rejects it.
    fn eval(&self, m: &Mod, cfg: &Cfg) -> Option<f64> {
        let mut value = m.value;
        for tag in &m.tags {
            match tag {
                Tag::Condition { var, neg } => {
                    if self.condition(var) == *neg {
                        return None;
                    }
                }
                Tag::Multiplier { var, div, limit, base } => {
                    let v = self.multipliers.get(var).copied().unwrap_or(0.0)
                        + self.sum_raw(Kind::Base, cfg, &format!("Multiplier:{var}"));
                    value = scale(value, v, *div, *limit, *base);
                }
                Tag::PerStat { stat, div, limit, base } => {
                    value = scale(value, self.stat(stat), *div, *limit, *base);
                }
                Tag::PercentStat { stat, percent } => {
                    // In this order: the floating point result decides the ceil, as in PoB2.
                    value = (value * (self.stat(stat) * (percent / 100.0))).ceil();
                }
                Tag::SkillType(t) => {
                    if !cfg.skill_types.contains(t) {
                        return None;
                    }
                }
                Tag::SlotName(s) => {
                    if cfg.slot != Some(*s) {
                        return None;
                    }
                }
                Tag::GlobalEffect { .. } => {}
            }
        }
        Some(value)
    }

    fn matches(m: &Mod, kind: Kind, cfg: &Cfg) -> bool {
        m.kind == kind && cfg.flags & m.flags == m.flags && keyword_match(cfg.keyword_flags, m.keyword_flags)
    }

    fn sum_raw(&self, kind: Kind, cfg: &Cfg, name: &str) -> f64 {
        let Some(list) = self.mods.get(name) else {
            return 0.0;
        };
        list.iter()
            .filter(|m| Self::matches(m, kind, cfg))
            .filter_map(|m| self.eval(m, cfg))
            .sum()
    }

    /// `modDB:Sum(kind, cfg, names...)`.
    pub fn sum(&self, kind: Kind, cfg: &Cfg, names: &[&str]) -> f64 {
        names.iter().map(|n| self.sum_raw(kind, cfg, n)).sum()
    }

    /// `modDB:More(cfg, names...)`.
    pub fn more(&self, cfg: &Cfg, names: &[&str]) -> f64 {
        let mut result = 1.0;
        for name in names {
            let Some(list) = self.mods.get(*name) else {
                continue;
            };
            let mut product = 1.0;
            for m in list.iter().filter(|m| Self::matches(m, Kind::More, cfg)) {
                if let Some(v) = self.eval(m, cfg) {
                    product *= 1.0 + v / 100.0;
                }
            }
            result = match HIGH_PRECISION_MORE.iter().find(|(n, _)| n == name) {
                Some(&(_, dec)) => {
                    let p = 10f64.powi(dec);
                    (result * product * p).floor() / p
                }
                None => result * round_to(product, 2),
            };
        }
        result
    }

    /// `modDB:Flag(cfg, names...)`.
    pub fn flag(&self, cfg: &Cfg, names: &[&str]) -> bool {
        names.iter().any(|n| {
            self.mods.get(*n).is_some_and(|l| {
                l.iter()
                    .filter(|m| Self::matches(m, Kind::Flag, cfg))
                    .any(|m| self.eval(m, cfg).is_some_and(|v| v != 0.0))
            })
        })
    }

    /// `modDB:Override(cfg, names...)`: the first matching value.
    pub fn override_value(&self, cfg: &Cfg, names: &[&str]) -> Option<f64> {
        names.iter().find_map(|n| {
            self.mods.get(*n)?.iter().filter(|m| Self::matches(m, Kind::Override, cfg)).find_map(|m| self.eval(m, cfg))
        })
    }

    /// `modDB:List(cfg, name)`: the text values.
    pub fn list(&self, cfg: &Cfg, name: &str) -> Vec<&str> {
        self.mods.get(name).map_or_else(Vec::new, |l| {
            l.iter()
                .filter(|m| Self::matches(m, Kind::List, cfg) && self.eval(m, cfg).is_some())
                .filter_map(|m| m.text.as_deref())
                .collect()
        })
    }

    /// `calcLib.mod`: `(1 + inc / 100) * more`.
    pub fn factor(&self, cfg: &Cfg, names: &[&str]) -> f64 {
        (1.0 + self.sum(Kind::Inc, cfg, names) / 100.0) * self.more(cfg, names)
    }

    /// `calcLib.val`: base times its factor.
    pub fn val(&self, cfg: &Cfg, name: &str) -> f64 {
        let base = self.sum(Kind::Base, cfg, &[name]);
        if base == 0.0 { 0.0 } else { base * self.factor(cfg, &[name]) }
    }
}

fn scale(value: f64, source: f64, div: f64, limit: Option<f64>, base: f64) -> f64 {
    let mut mult = (source / div + 0.0001).floor();
    if let Some(l) = limit {
        mult = mult.min(l);
    }
    value * mult + base
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Cfg = Cfg { flags: 0, keyword_flags: 0, slot: None, skill_types: &[] };

    #[test]
    fn more_rounds_the_product_to_two_decimals() {
        let mut db = ModDb::default();
        db.new_mod("Life", Kind::More, 10.0);
        db.new_mod("Life", Kind::More, 7.0);
        // 1.1 * 1.07 = 1.177
        assert_eq!(db.more(&NONE, &["Life"]), 1.18);
    }

    #[test]
    fn high_precision_more_floors() {
        let mut db = ModDb::default();
        db.new_mod("ReservationMultiplier", Kind::More, 11.111);
        assert_eq!(db.more(&NONE, &["ReservationMultiplier"]), 1.1111);
    }

    #[test]
    fn flags_are_all_required_of_the_query() {
        let mut db = ModDb::default();
        db.add(Mod::new("Speed", Kind::Inc, 10.0).with_flags(flag::ATTACK));
        assert_eq!(db.sum(Kind::Inc, &NONE, &["Speed"]), 0.0);
        let attack = Cfg { flags: flag::ATTACK | flag::HIT, ..NONE };
        assert_eq!(db.sum(Kind::Inc, &attack, &["Speed"]), 10.0);
    }

    #[test]
    fn keyword_match_all_and_any() {
        assert!(keyword_match(keyword::ATTACK, 0));
        assert!(keyword_match(keyword::ATTACK | keyword::HIT, keyword::ATTACK));
        assert!(!keyword_match(keyword::HIT, keyword::ATTACK));
        assert!(!keyword_match(keyword::HIT, keyword::ATTACK | keyword::HIT | keyword::MATCH_ALL));
    }

    #[test]
    fn multiplier_floors_and_adds_base() {
        let mut db = ModDb::default();
        db.multipliers.insert("Level", 20.0);
        db.add(Mod::new("Life", Kind::Base, 12.0).with_tag(Tag::Multiplier { var: "Level", div: 1.0, limit: None, base: 16.0 }));
        assert_eq!(db.sum(Kind::Base, &NONE, &["Life"]), 256.0);
    }

    #[test]
    fn per_stat_and_percent_stat() {
        let mut db = ModDb::default();
        db.stats.insert("Dex", 25.0);
        db.stats.insert("Life", 65.0);
        db.add(Mod::new("Speed", Kind::Inc, 2.0).with_tag(Tag::per_stat("Dex", 10.0)));
        db.add(Mod::new("AilmentThreshold", Kind::Base, 50.0).with_tag(Tag::PercentStat { stat: "Life", percent: 1.0 }));
        assert_eq!(db.sum(Kind::Inc, &NONE, &["Speed"]), 4.0);
        assert_eq!(db.sum(Kind::Base, &NONE, &["AilmentThreshold"]), 33.0);
    }

    #[test]
    fn conditions_and_negation() {
        let mut db = ModDb::default();
        db.add(Mod::new("Life", Kind::Inc, 5.0).with_tag(Tag::condition("UsingShield")));
        db.add(Mod::new("Life", Kind::Inc, 3.0).with_tag(Tag::Condition { var: "UsingShield", neg: true }));
        assert_eq!(db.sum(Kind::Inc, &NONE, &["Life"]), 3.0);
        db.conditions.insert("UsingShield", true);
        assert_eq!(db.sum(Kind::Inc, &NONE, &["Life"]), 5.0);
    }

    #[test]
    fn slot_and_skill_type_tags() {
        let mut db = ModDb::default();
        db.add(Mod::new("Armour", Kind::Inc, 5.0).with_tag(Tag::SlotName("Helmet")));
        db.add(Mod::new("Damage", Kind::Inc, 5.0).with_tag(Tag::SkillType(1)));
        assert_eq!(db.sum(Kind::Inc, &NONE, &["Armour"]), 0.0);
        assert_eq!(db.sum(Kind::Inc, &Cfg { slot: Some("Helmet"), ..NONE }, &["Armour"]), 5.0);
        assert_eq!(db.sum(Kind::Inc, &Cfg { skill_types: &[1], ..NONE }, &["Damage"]), 5.0);
    }

    #[test]
    fn override_and_list() {
        let mut db = ModDb::default();
        db.new_mod("Life", Kind::Override, 1.0);
        let mut l = Mod::new("Text", Kind::List, 0.0);
        l.text = Some("a".into());
        db.add(l);
        assert_eq!(db.override_value(&NONE, &["Life"]), Some(1.0));
        assert_eq!(db.list(&NONE, "Text"), ["a"]);
    }
}
