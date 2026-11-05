//! What the server sends: the calculator's numbers as game stats. Only stats whose formula fits
//! the official captures (`cargo xtask stats-sent`) are here, for a character without gear; which
//! other stats the official server sends and why: hub `docs/architecture/stats.md`.

use crate::{Output, calc::CharacterBase, round};

/// The stats of a character's own list (the Stats component's base list, `StatsUpdate` set 0).
pub fn own_stats(out: &Output, class: &CharacterBase) -> Vec<(&'static str, i32)> {
    let per_minute = |per_second: f64| round(per_second * 60.0) as i32;
    let i = |v: f64| v as i32;
    let highest = out.str.max(out.dex).max(out.int);
    vec![
        ("strength", i(out.str)),
        ("dexterity", i(out.dex)),
        ("intelligence", i(out.int)),
        ("total_attributes", i(out.str + out.dex + out.int)),
        ("starting_attribute_strength", i(class.strength)),
        ("starting_attribute_dexterity", i(class.dexterity)),
        ("starting_attribute_intelligence", i(class.intelligence)),
        ("virtual_strength_for_requirements", i(out.str)),
        ("virtual_dexterity_for_requirements", i(out.dex)),
        ("virtual_intelligence_for_requirements", i(out.int)),
        ("virtual_strength_for_gem_requirements", i(out.str)),
        ("virtual_dexterity_for_gem_requirements", i(out.dex)),
        ("virtual_intelligence_for_gem_requirements", i(out.int)),
        ("virtual_gem_requirements_can_be_satisfied_by_highest_attribute_value", i(highest)),
        ("maximum_life", i(out.life)),
        ("life_unreserved", i(out.life)),
        ("maximum_mana", i(out.mana)),
        ("mana_unreserved", i(out.mana)),
        ("maximum_energy_shield", i(out.energy_shield)),
        ("total_mana_recovery_per_minute_from_regeneration", per_minute(out.mana_regen_raw)),
        ("mana_recovery_per_minute", per_minute(out.mana_regen_raw)),
        ("energy_shield_recharge_rate_per_minute", per_minute(out.es_recharge_raw)),
        ("ailment_threshold", i(out.ailment_threshold)),
        ("virtual_stun_threshold", i(out.stun_threshold)),
        ("base_movement_velocity_+%", i(out.movement_inc_percent)),
        ("combined_movement_velocity_+%_final", i(out.movement_final_percent)),
    ]
}
