//! Character stats.
//!
//! The client keys stats by `Stats.dat` row + 1 ([`StatRegistry`]). A freshly created
//! character's stats come from:
//! - the object template's `Stats` section (`Metadata/Characters/Character.ot`: `level`,
//!   `is_player`, `life_per_level`, ...);
//! - the class row in `Characters.dat` (base life/mana and attributes);
//! - the character's level.
//!
//! The derived totals come from the calculator (`poe-stats`, [`derive`]); items, passives and
//! buffs reach it with the stats pipeline of P10.

use std::collections::{BTreeMap, HashMap};

use poe_data::{GameData, Result, template::ObjectTemplate, views::CharacterClass};

/// Stat id (`Stats.Id`) -> network key, loaded once.
pub struct StatRegistry {
    keys: HashMap<String, u32>,
    /// Stats the server sends to the other players too (`Stats.IsParty`); the rest only go to
    /// the entity's own client (official captures: `stun_threshold_+%_final_while_performing_action`
    /// is in a local roll batch but never in a remote one).
    party: std::collections::HashSet<u32>,
}

impl StatRegistry {
    pub fn load(data: &GameData) -> Result<Self> {
        let table = data.table("Stats")?;
        let mut keys = HashMap::with_capacity(table.len());
        let mut party = std::collections::HashSet::new();
        for i in 0..table.len() {
            if let Some(row) = table.row(i) {
                keys.insert(row.str("Id")?, i as u32 + 1);
                if row.bool("IsParty")? {
                    party.insert(i as u32 + 1);
                }
            }
        }
        Ok(Self { keys, party })
    }

    /// A registry from explicit rows `(stat id, network key, IsParty)`, for tests and tools that
    /// have no game files.
    pub fn from_rows(rows: &[(&str, u32, bool)]) -> Self {
        Self {
            keys: rows
                .iter()
                .map(|&(id, key, _)| (id.to_owned(), key))
                .collect(),
            party: rows.iter().filter(|r| r.2).map(|r| r.1).collect(),
        }
    }

    /// Whether other players' clients are told about this stat (`Stats.IsParty`).
    pub fn is_party(&self, id: &str) -> bool {
        self.key(id).is_some_and(|k| self.party.contains(&k))
    }

    pub fn key(&self, id: &str) -> Option<u32> {
        self.keys.get(id).copied()
    }
}

/// Stat values by id, in a stable order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats(pub BTreeMap<String, i32>);

impl Stats {
    pub fn get(&self, id: &str) -> i32 {
        self.0.get(id).copied().unwrap_or(0)
    }

    pub fn set(&mut self, id: &str, value: i32) {
        self.0.insert(id.to_owned(), value);
    }

    /// (network key, value) pairs; stats unknown to `registry` or zero are skipped.
    pub fn to_network(&self, registry: &StatRegistry) -> Vec<(u32, i32)> {
        self.0
            .iter()
            .filter(|(_, v)| **v != 0)
            .filter_map(|(id, v)| {
                let key = registry.key(id);
                if key.is_none() {
                    tracing::debug!(stat = %id, "stat not in Stats.dat");
                }
                Some((key?, *v))
            })
            .collect()
    }
}

/// The engine's default energy shield recharge delay in milliseconds (the stat
/// `virtual_total_energy_shield_recharge_delay_ms` of every official character without delay
/// modifiers; 2000 with `energy_shield_delay_-%` 100). Not in any game table.
pub const ES_RECHARGE_DELAY_MS: i32 = 4000;

/// Adds the derived stats other players' clients get for a character that stands at full life,
/// mana and energy shield with nothing equipped (the capture of an official town lists them for
/// every player): the "on full ..." conditions, `poise_threshold` and
/// `intermediary_poise_threshold` (= maximum life) and the energy shield recharge delay.
/// Everything else the official server derives (armour, evasion, accuracy, regeneration, ...)
/// needs a stat calculator and is left out.
pub fn add_resting_state(stats: &mut Stats) {
    for id in [
        "virtual_on_full_life",
        "on_full_mana",
        "on_full_energy_shield",
        "above_90%_mana",
    ] {
        stats.set(id, 1);
    }
    let life = stats.get("maximum_life");
    stats.set("poise_threshold", life);
    stats.set("intermediary_poise_threshold", life);
    let delay_less = stats.get("energy_shield_delay_-%").max(0) as i64;
    let delay = i64::from(ES_RECHARGE_DELAY_MS) * 100 / (100 + delay_less);
    stats.set(
        "virtual_total_energy_shield_recharge_delay_ms",
        delay as i32,
    );
}

/// Default stats of a new character of `class` at `level`.
pub fn character_defaults(template: &ObjectTemplate, class: &CharacterClass, level: u32) -> Stats {
    let mut stats = Stats::default();
    for (id, value) in template.component("Stats") {
        let v = match value.as_str() {
            "true" => 1,
            "false" => 0,
            v => match v.parse::<i64>() {
                Ok(n) => n.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
                // Non-integer template values (none in Character.ot today) are ignored.
                Err(_) => continue,
            },
        };
        stats.set(id, v);
    }
    let level = level.max(1) as i32;
    stats.set("level", level);
    stats.set("base_maximum_life", class.base_max_life as i32);
    stats.set("base_maximum_mana", class.base_max_mana as i32);
    stats.set("base_strength", class.base_strength as i32);
    stats.set("base_dexterity", class.base_dexterity as i32);
    stats.set("base_intelligence", class.base_intelligence as i32);

    derive(&mut stats, class);
    let speed = speed_stats(&stats);
    if speed.permyriad(0) != 0 {
        stats.set("movement_velocity_+permyriad", speed.permyriad(0));
    }
    stats
}

/// The calculator's input for a character with nothing equipped, from the template stats and the
/// class row.
fn calculator_input(stats: &Stats, class: &CharacterClass) -> poe_stats::Input {
    let template = |id: &str| stats.0.get(id).map(|v| f64::from(*v));
    poe_stats::Input {
        constants: poe_stats::Constants::from_template(template, class.base_max_life as f64, class.base_max_mana as f64),
        class: poe_stats::CharacterBase {
            strength: class.base_strength as f64,
            dexterity: class.base_dexterity as f64,
            intelligence: class.base_intelligence as f64,
            unarmed: poe_stats::WeaponBase {
                kind: poe_stats::WeaponType::Unarmed,
                physical_min: class.min_damage as f64,
                physical_max: class.max_damage as f64,
                attack_rate: poe_stats::round_to(1000.0 / (class.weapon_speed as f64).max(1.0), 2),
                crit_chance: template("unarmed_base_critical_strike_chance").unwrap_or(0.0) / 100.0,
                reload_time: 0.0,
            },
        },
        level: stats.get("level").max(1) as u32,
        items: Vec::new(),
        passives: Vec::new(),
        extra_mods: Vec::new(),
        resist_penalty: 0.0,
        skill: poe_stats::SkillProfile::default_attack(poe_stats::WeaponType::Unarmed, 1.0),
        enemy: poe_stats::Enemy::default(),
        conditions: Vec::new(),
    }
}

/// Runs the calculator and stores what the server sends for the character.
fn derive(stats: &mut Stats, class: &CharacterClass) {
    let input = calculator_input(stats, class);
    let out = poe_stats::calculate(&input);
    for (id, value) in poe_stats::network::own_stats(&out, &input.class) {
        stats.set(id, value);
    }
}

/// The character's stats that change its speed.
pub fn speed_stats(stats: &Stats) -> crate::movement::SpeedStats {
    crate::movement::SpeedStats {
        increased_percent: stats.get("base_movement_velocity_+%"),
        final_percent: stats.get("combined_movement_velocity_+%_final"),
        action_speed_percent: stats.get("virtual_action_speed_+%"),
        velocity_cap: stats.get("movement_velocity_cap"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every pair the official server sent (2026-10-20, 2026-11-02 captures).
    #[test]
    fn the_speed_stat_matches_the_official_values() {
        let s = |increased_percent, final_percent| crate::movement::SpeedStats {
            increased_percent,
            final_percent,
            ..Default::default()
        };
        for (stats, walk, sprint) in [
            (s(0, 0), 0, 5000),
            (s(0, -4), -400, 4400),
            (s(0, -3), -300, 4600),
            (s(10, -4), 600, 5400),
        ] {
            assert_eq!((stats.permyriad(0), stats.permyriad(50)), (walk, sprint));
        }
    }

    #[test]
    fn resting_state_adds_the_town_conditions() {
        let mut stats = Stats::default();
        stats.set("maximum_life", 188);
        add_resting_state(&mut stats);
        for id in [
            "virtual_on_full_life",
            "on_full_mana",
            "on_full_energy_shield",
        ] {
            assert_eq!(stats.get(id), 1, "{id}");
        }
        assert_eq!(stats.get("poise_threshold"), 188);
        assert_eq!(stats.get("intermediary_poise_threshold"), 188);
        // 4000 ms; 2000 with `energy_shield_delay_-%` 100 (the official Witch).
        let delay = "virtual_total_energy_shield_recharge_delay_ms";
        assert_eq!(stats.get(delay), 4000);
        stats.set("energy_shield_delay_-%", 100);
        add_resting_state(&mut stats);
        assert_eq!(stats.get(delay), 2000);
    }

    #[test]
    fn registry_rows_mark_public_stats() {
        let registry = StatRegistry::from_rows(&[("a", 1, true), ("b", 2, false)]);
        assert!(registry.is_party("a"));
        assert!(!registry.is_party("b"));
        assert!(!registry.is_party("missing"));
        assert_eq!(registry.key("b"), Some(2));
    }

    #[test]
    fn ranger_level_one() {
        let mut template = ObjectTemplate::default();
        template.components.insert(
            "Stats".into(),
            vec![
                ("level".into(), "1".into()),
                ("is_player".into(), "1".into()),
                ("life_per_level".into(), "12".into()),
                ("mana_per_level".into(), "4".into()),
                ("character_inherent_mana_regeneration_rate_per_minute_%".into(), "240".into()),
                ("enable_weapon_sets".into(), "true".into()),
            ],
        );
        let class = CharacterClass {
            metadata_id: "Metadata/Characters/Dex/DexFour".into(),
            name: "Ranger".into(),
            integer_id: 2,
            base_max_life: 16,
            base_max_mana: 30,
            ao_file: "Metadata/Characters/Dex/DexFour.ao".into(),
            base_strength: 7,
            base_dexterity: 15,
            base_intelligence: 7,
            min_damage: 2,
            max_damage: 6,
            weapon_speed: 833,
        };
        // The official server's numbers for this character (captures, stats-sent).
        let s = character_defaults(&template, &class, 1);
        assert_eq!(s.get("maximum_life"), 42);
        assert_eq!(s.get("maximum_mana"), 48);
        assert_eq!(s.get("life_unreserved"), 42);
        assert_eq!(s.get("ailment_threshold"), 21);
        assert_eq!(s.get("virtual_stun_threshold"), 42);
        assert_eq!(s.get("total_mana_recovery_per_minute_from_regeneration"), 115);
        assert_eq!(s.get("total_attributes"), 29);
        assert_eq!(s.get("dexterity"), 15);
        let s = character_defaults(&template, &class, 2);
        assert_eq!(s.get("maximum_life"), 54);
        assert_eq!(s.get("maximum_mana"), 52);
        assert_eq!(s.get("ailment_threshold"), 27);
        assert_eq!(s.get("total_mana_recovery_per_minute_from_regeneration"), 125);
        assert_eq!(s.get("is_player"), 1);
        assert_eq!(s.get("enable_weapon_sets"), 1);
    }
}
