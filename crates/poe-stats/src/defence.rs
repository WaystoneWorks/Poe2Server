//! Defences: PoB2's `CalcDefence.lua` for the basic stats (pools, armour, evasion, energy shield,
//! resistances, block, regeneration, recharge, movement and action speed, thresholds). Hit
//! mitigation for the damage pipeline is [`armour_reduction`] and [`hit_taken`].

use crate::{
    calc::{Env, NO_CFG, Output},
    moddb::Kind,
    round, round_to,
};

pub const ACCURACY_PER_DEX: f64 = 6.0;
const RESIST_FLOOR: f64 = -200.0;
const MAX_RESIST_CAP: f64 = 90.0;
const BLOCK_CAP: f64 = 90.0;
/// `data.misc.ArmourRatio`.
const ARMOUR_RATIO: f64 = 10.0;

/// `calcs.armourReductionF`: percent of a raw physical hit that armour stops.
pub fn armour_reduction(armour: f64, raw: f64) -> f64 {
    if armour == 0.0 && raw == 0.0 {
        0.0
    } else if armour < 0.0 {
        -(-armour / (-armour + raw * ARMOUR_RATIO) * 100.0)
    } else {
        armour / (armour + raw * ARMOUR_RATIO) * 100.0
    }
}

/// A hit after armour (physical, `resist` is the extra reduction) or after resistance.
pub fn hit_taken(raw: f64, physical: bool, armour: f64, resist: f64) -> f64 {
    let reduction = if physical { (armour_reduction(armour, raw) + resist).min(MAX_RESIST_CAP) } else { resist };
    raw * (1.0 - reduction / 100.0)
}

/// `calcs.doActorLifeManaSpirit`.
pub(crate) fn pools(env: &mut Env, out: &mut Output) {
    for res in ["Life", "Mana", "Spirit"] {
        let db = &env.db;
        let base = db.sum(Kind::Base, &NO_CFG, &[res]);
        let extra = db.sum(Kind::Base, &NO_CFG, &[&format!("Extra{res}")]);
        let total = db.sum(Kind::Base, &NO_CFG, &[&format!("{res}Total")]);
        let inc = db.sum(Kind::Inc, &NO_CFG, &[res]);
        let more = db.more(&NO_CFG, &[res]);
        let value = db
            .override_value(&NO_CFG, &[res])
            .unwrap_or_else(|| round((base + extra) * (1.0 + inc / 100.0) * more + total).max(1.0));
        match res {
            "Life" => {
                out.life = value;
                env.db.stats.insert("Life", value);
            }
            "Mana" => {
                out.mana = value;
                env.db.stats.insert("Mana", value);
            }
            _ => {
                out.spirit = value;
                env.db.stats.insert("Spirit", value);
            }
        }
    }
}

fn resist(env: &Env, elem: &str, elemental: bool) -> (f64, f64) {
    let db = &env.db;
    let name = format!("{elem}Resist");
    let names: &[&str] = if elemental { &[&name, "ElementalResist"] } else { &[&name] };
    let total = db.override_value(&NO_CFG, &[&name]).unwrap_or_else(|| {
        let base = db.sum(Kind::Base, &NO_CFG, names);
        let factor = db.factor(&NO_CFG, names).max(0.0);
        base * factor
    });
    let total = total.trunc();
    let max_name = format!("{elem}ResistMax");
    let max_names: &[&str] = if elemental { &[&max_name, "ElementalResistMax"] } else { &[&max_name] };
    let max = db
        .override_value(&NO_CFG, &[&max_name])
        .unwrap_or_else(|| db.sum(Kind::Base, &NO_CFG, max_names).min(MAX_RESIST_CAP))
        .trunc();
    (total.min(max).max(RESIST_FLOOR), max)
}

/// The regeneration loop of `calcs.defence`.
fn regen(env: &Env, resource: &str, pool: f64) -> f64 {
    let db = &env.db;
    let regen_name = format!("{resource}Regen");
    let rate_name = format!("{resource}RecoveryRate");
    let inc = db.sum(Kind::Inc, &NO_CFG, &[&regen_name, &rate_name]);
    let more = db.more(&NO_CFG, &[&regen_name, &rate_name]);
    let base = db.sum(Kind::Base, &NO_CFG, &[&regen_name])
        + pool * db.sum(Kind::Base, &NO_CFG, &[&format!("{resource}RegenPercent")]) / 100.0;
    round_to(base * (1.0 + inc / 100.0) * more, 1)
}

/// `calcs.defence`, the basic stats.
pub(crate) fn defence(env: &mut Env, out: &mut Output) {
    out.action_speed_mod = {
        let inc = env.db.sum(Kind::Inc, &NO_CFG, &["ActionSpeed"]);
        (1.0 + inc / 100.0).max(0.0)
    };
    (out.fire_resist, out.fire_resist_max) = resist(env, "Fire", true);
    (out.cold_resist, out.cold_resist_max) = resist(env, "Cold", true);
    (out.lightning_resist, out.lightning_resist_max) = resist(env, "Lightning", true);
    (out.chaos_resist, out.chaos_resist_max) = resist(env, "Chaos", false);

    let db = &env.db;
    out.block_chance_max = db
        .override_value(&NO_CFG, &["BlockChanceMax"])
        .unwrap_or_else(|| db.sum(Kind::Base, &NO_CFG, &["BaseBlockChanceMax"]) + db.sum(Kind::Base, &NO_CFG, &["BlockChanceMax"]))
        .min(BLOCK_CAP);
    let shield_block: f64 = env.off_hand_armour.as_ref().map_or(0.0, |a| a.block);
    let block_inc = db.sum(Kind::Inc, &NO_CFG, &["BlockChance"]);
    let block_more = db.more(&NO_CFG, &["BlockChance"]);
    let total_block = round((shield_block + db.sum(Kind::Base, &NO_CFG, &["BlockChance"])) * (1.0 + block_inc / 100.0) * block_more);
    out.block_chance = total_block.min(out.block_chance_max);

    // Armour, evasion, energy shield: the item numbers count as base, then the global mods apply.
    let slots_a: f64 = env.armour_items.iter().map(|a| a.armour).sum();
    let slots_e: f64 = env.armour_items.iter().map(|a| a.evasion).sum();
    let slots_s: f64 = env.armour_items.iter().map(|a| a.energy_shield).sum();
    let slots_w: f64 = env.armour_items.iter().map(|a| a.ward).sum();
    let pool = |slot_base: f64, names: &[&str], total: &str| {
        let base = db.override_value(&NO_CFG, names).unwrap_or_else(|| db.sum(Kind::Base, &NO_CFG, names));
        (slot_base + base) * db.factor(&NO_CFG, names) + db.sum(Kind::Base, &NO_CFG, &[total])
    };
    out.armour = round(pool(slots_a, &["Armour", "ArmourAndEvasion", "Defences"], "ArmourTotal")).max(0.0);
    out.evasion = round(pool(slots_e, &["Evasion", "ArmourAndEvasion", "Defences"], "EvasionTotal")).max(0.0);
    out.energy_shield = db
        .override_value(&NO_CFG, &["EnergyShield"])
        .unwrap_or_else(|| round(pool(slots_s, &["EnergyShield", "Defences"], "EnergyShieldTotal")).max(0.0));
    out.ward = round(pool(slots_w, &["Ward", "Defences"], "WardTotal")).max(0.0);

    out.mana_regen = regen(env, "Mana", out.mana);
    out.life_regen = regen(env, "Life", out.life);
    out.es_regen = regen(env, "EnergyShield", out.energy_shield);
    let recharge_inc = db.sum(Kind::Inc, &NO_CFG, &["EnergyShieldRecharge"]);
    let recharge_more = db.more(&NO_CFG, &["EnergyShieldRecharge"]);
    out.es_recharge = round_to(out.energy_shield * env.es_recharge_rate * (1.0 + recharge_inc / 100.0) * recharge_more, 1);

    let base_ms = db.sum(Kind::Base, &NO_CFG, &["MovementSpeed"]);
    out.movement_speed_mod = db
        .override_value(&NO_CFG, &["MovementSpeed"])
        .unwrap_or_else(|| round_to((1.0 + base_ms) * db.factor(&NO_CFG, &["MovementSpeed"]), 3));

    let stun_inc = 1.0 + db.sum(Kind::Inc, &NO_CFG, &["StunThreshold"]) / 100.0;
    let stun_more = db.more(&NO_CFG, &["StunThreshold"]);
    out.stun_threshold = (out.life + db.sum(Kind::Base, &NO_CFG, &["StunThreshold"])) * stun_inc * stun_more;
    out.ailment_threshold = db.val(&NO_CFG, "AilmentThreshold");

    out.item_rarity = db.sum(Kind::Inc, &NO_CFG, &["LootRarity"]);
    out.item_quantity = db.sum(Kind::Inc, &NO_CFG, &["LootQuantity"]);
    out.light_radius = db.sum(Kind::Inc, &NO_CFG, &["LightRadius"]);
    out.charm_slots = db.sum(Kind::Base, &NO_CFG, &["CharmLimit"]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn armour_reduction_matches_the_formula() {
        assert_eq!(armour_reduction(0.0, 0.0), 0.0);
        assert!((armour_reduction(100.0, 10.0) - 50.0).abs() < 1e-9);
        assert!((armour_reduction(-100.0, 10.0) + 50.0).abs() < 1e-9);
    }

    #[test]
    fn hits_after_mitigation() {
        // 100 armour against a raw 10 hit stops half of it.
        assert!((hit_taken(10.0, true, 100.0, 0.0) - 5.0).abs() < 1e-9);
        // Physical reduction is capped at 90%.
        assert!((hit_taken(10.0, true, 100.0, 80.0) - 1.0).abs() < 1e-9);
        assert!((hit_taken(100.0, false, 0.0, 75.0) - 25.0).abs() < 1e-9);
    }
}
