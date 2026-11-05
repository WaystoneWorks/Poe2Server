//! Offence: PoB2's `CalcOffence.lua` for one weapon hit (accuracy, hit chance, speed, crit, hit
//! damage against an [`Enemy`](crate::Enemy)). Extended by the skills step (P16); conversion,
//! dual wielding and damage over time are not here yet.

use crate::{
    calc::{Env, Input, NO_CFG, Output},
    defence::armour_reduction,
    items::{DAMAGE_TYPES, WeaponType},
    moddb::{Cfg, Kind, flag, keyword},
    round, round_to,
};

/// `data.misc.NegArmourDmgBonusCap`.
const NEG_ARMOUR_DAMAGE_BONUS_CAP: f64 = 100.0;

/// The active skill as far as the calculator needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillProfile {
    /// `skillModFlags` without the weapon's flags.
    pub flags: u64,
    pub keyword_flags: u32,
    pub skill_types: Vec<u32>,
    /// `grantedEffectLevel.baseMultiplier`.
    pub base_multiplier: f64,
    /// `base_number_of_crossbow_bolts` of the skill at its level, for a crossbow skill.
    pub base_bolts: f64,
}

impl SkillProfile {
    /// The default attack of a weapon type: flags and skill types of the skill PoB2 selects
    /// (Bow Shot, Crossbow Shot, Mace Strike, Spear Stab, Quarterstaff Strike, otherwise Punch).
    /// `base_multiplier` depends on the skill's level: from game data once skills exist (P16).
    pub fn default_attack(weapon: WeaponType, base_multiplier: f64) -> Self {
        let common = flag::HIT | flag::ATTACK;
        let kw = keyword::ATTACK | keyword::HIT;
        match weapon {
            WeaponType::Bow => Self {
                flags: common | flag::PROJECTILE,
                keyword_flags: kw | keyword::BOW | keyword::ARROW,
                skill_types: vec![1, 3, 41, 57, 58, 70, 141, 184],
                base_multiplier,
                base_bolts: 1.0,
            },
            WeaponType::Crossbow => Self {
                flags: common | flag::PROJECTILE | flag::AREA,
                keyword_flags: kw,
                skill_types: vec![1, 3, 14, 15, 16, 41, 57, 116, 141],
                base_multiplier,
                base_bolts: 1.0,
            },
            _ => Self {
                flags: common | flag::MELEE | flag::AREA,
                keyword_flags: kw,
                skill_types: vec![1, 8, 20, 21, 83],
                base_multiplier,
                base_bolts: 1.0,
            },
        }
    }
}

/// `calcs.hitChance`.
pub fn hit_chance(evasion: f64, accuracy: f64) -> f64 {
    if accuracy < 0.0 {
        return 5.0;
    }
    round(accuracy * 1.25 / (accuracy + evasion * 0.3) * 100.0).clamp(5.0, 100.0)
}

fn damage_names(ty: &str) -> Vec<String> {
    let mut names = vec!["Damage".to_owned(), format!("{ty}Damage")];
    if matches!(ty, "Fire" | "Cold" | "Lightning") {
        names.push("ElementalDamage".to_owned());
    }
    names
}

/// `calcs.offence` for the main hand of a single weapon.
pub(crate) fn offence(env: &mut Env, input: &Input, out: &mut Output) {
    let skill = &input.skill;
    let cfg = Cfg {
        flags: skill.flags | env.weapon_kind.mod_flags(),
        keyword_flags: skill.keyword_flags,
        slot: None,
        skill_types: &skill.skill_types,
    };
    let db = &env.db;
    let enemy = &input.enemy;

    // Accuracy. PoB2 queries the `More` part without the skill's flags.
    let base = db.sum(Kind::Base, &cfg, &["Accuracy"]);
    let inc = db.sum(Kind::Inc, &cfg, &["Accuracy"]);
    let more = db.more(&NO_CFG, &["Accuracy"]);
    out.accuracy = (base * (1.0 + inc / 100.0) * more).floor().max(0.0);
    out.hit_chance = hit_chance(enemy.evasion, out.accuracy);

    // Speed.
    let base_time = 1.0 / env.weapon.attack_rate + db.sum(Kind::Base, &cfg, &["Speed"]);
    let speed_inc = db.sum(Kind::Inc, &cfg, &["Speed"]);
    let speed_more = db.more(&cfg, &["Speed"]);
    out.speed = 1.0 / (base_time / round_to((1.0 + speed_inc / 100.0) * speed_more, 2)) * out.action_speed_mod;
    if env.weapon.reload_time > 0.0 {
        // The firing rate and the reload of one clip (`calcCrossbowAmmoStats`, the `reloadTime` part
        // of `calcs.offence`).
        let reload = env.weapon.reload_time / db.factor(&cfg, &["ReloadSpeed", "Speed"]);
        let bolts = (skill.base_bolts + db.sum(Kind::Base, &cfg, &["CrossbowBoltCount"])).max(1.0);
        let not_consumed = db.sum(Kind::Base, &cfg, &["ChanceToNotConsumeAmmo"]);
        let instant = db.sum(Kind::Base, &cfg, &["InstantReloadChance"]).min(100.0);
        if not_consumed < 100.0 {
            let effective_bolts = bolts / (1.0 - not_consumed / 100.0);
            let firing_time = effective_bolts / out.speed;
            out.speed = 1.0 / ((firing_time + reload * (1.0 - instant / 100.0)) / effective_bolts);
        }
    }

    // Crit.
    let crit_base = db.sum(Kind::Base, &cfg, &["CritChance"]);
    let crit_inc = db.sum(Kind::Inc, &cfg, &["CritChance"]);
    let crit_more = db.more(&cfg, &["CritChance"]);
    let crit = round((env.weapon.crit_chance + crit_base) * (1.0 + crit_inc / 100.0) * crit_more * 100.0) / 100.0;
    let crit = crit.min(db.sum(Kind::Base, &cfg, &["CritChanceCap"])).max(0.0);
    out.crit_chance_pre_hit = crit;
    out.crit_chance = crit * out.hit_chance / 100.0;
    let extra = db.sum(Kind::Base, &cfg, &["CritMultiplier"]) / 100.0
        * (1.0 + db.sum(Kind::Inc, &cfg, &["CritMultiplier"]) / 100.0)
        * db.more(&NO_CFG, &["CritMultiplier"]);
    out.crit_multiplier = 1.0 + extra.max(0.0);

    // Hit damage, one pass for a crit and one for a normal hit.
    let mut hit_avg = 0.0;
    let mut crit_avg = 0.0;
    for (i, ty) in DAMAGE_TYPES.iter().enumerate() {
        let added_mult = db.factor(&cfg, &[&format!("Added{ty}Damage"), "AddedDamage"]);
        let added_min = db.sum(Kind::Base, &cfg, &[&format!("{ty}Min")]);
        let added_max = db.sum(Kind::Base, &cfg, &[&format!("{ty}Max")]);
        let (wmin, wmax) = env.weapon.damage[i];
        let summed_min = (wmin + added_min * added_mult) * skill.base_multiplier;
        let summed_max = (wmax + added_max * added_mult) * skill.base_multiplier;
        if summed_min == 0.0 && summed_max == 0.0 {
            continue;
        }
        let names = damage_names(ty);
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let dmg_inc = 1.0 + db.sum(Kind::Inc, &cfg, &names) / 100.0;
        let dmg_more = db.more(&cfg, &names);
        let min = round(summed_min * dmg_inc * dmg_more);
        let max = round(summed_max * dmg_inc * dmg_more);
        for pass in 0..2 {
            let mult = if pass == 0 { out.crit_multiplier } else { 1.0 };
            let avg = (min * mult + max * mult) / 2.0;
            let resist = if i == 0 {
                (enemy.physical_reduction + armour_reduction(enemy.armour, avg)).max(-NEG_ARMOUR_DAMAGE_BONUS_CAP).min(enemy.max_physical_reduction)
            } else {
                enemy.resist[i - 1]
            };
            let after = avg * (1.0 - resist / 100.0);
            if pass == 0 {
                crit_avg += after;
            } else {
                hit_avg += after;
            }
        }
    }
    out.average_hit = hit_avg * (1.0 - out.crit_chance / 100.0) + crit_avg * out.crit_chance / 100.0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_chance_is_clamped() {
        assert_eq!(hit_chance(0.0, 100.0), 100.0);
        assert_eq!(hit_chance(1000.0, 0.0), 5.0);
        assert_eq!(hit_chance(100.0, -1.0), 5.0);
    }

    #[test]
    fn default_attack_flags_match_pob() {
        let bow = SkillProfile::default_attack(WeaponType::Bow, 1.53);
        assert_eq!(bow.flags | WeaponType::Bow.mod_flags(), 0xa_0002_2405);
        let punch = SkillProfile::default_attack(WeaponType::Unarmed, 1.0);
        assert_eq!(punch.flags | WeaponType::Unarmed.mod_flags(), 0x100_0305);
        let claw = SkillProfile::default_attack(WeaponType::Claw, 1.0);
        assert_eq!(claw.flags | WeaponType::Claw.mod_flags(), 0x5_0004_2305);
    }
}
