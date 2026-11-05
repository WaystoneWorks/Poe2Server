//! The calculation pipeline: PoB2's `calcs.initEnv` base mods (`CalcSetup.lua`), then
//! `calcs.perform` (`CalcPerform.lua`): attributes, pools, defences, offence.

use crate::{
    SkillProfile, defence,
    items::{self, ItemInput, WeaponBase, WeaponData, WeaponType},
    map,
    moddb::{Cfg, Kind, Mod, ModDb, Tag},
    offence, round,
};

/// Constants of the `Character` object template (`data.characterConstants`, from game data).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Constants {
    pub life_per_level: f64,
    pub mana_per_level: f64,
    pub accuracy_per_level: f64,
    pub base_evasion: f64,
    /// `base_critical_hit_damage_bonus`, percent.
    pub base_crit_bonus: f64,
    pub max_resistance: f64,
    pub max_block: f64,
    /// `character_inherent_mana_regeneration_rate_per_minute_%`.
    pub mana_regen_per_minute: f64,
    /// `energy_shield_recharge_rate_per_minute_%`.
    pub es_recharge_per_minute: f64,
    /// The life and mana a level 0 character would have (PoB2's `base = 16` and `base = 30`).
    pub base_life: f64,
    pub base_mana: f64,
}

/// The class row of `Characters.dat`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CharacterBase {
    pub strength: f64,
    pub dexterity: f64,
    pub intelligence: f64,
    pub unarmed: WeaponBase,
}

/// The target of the offence numbers. `Enemy::default()` does not mitigate anything.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Enemy {
    pub evasion: f64,
    pub armour: f64,
    pub physical_reduction: f64,
    /// `[Lightning, Cold, Fire, Chaos]`.
    pub resist: [f64; 4],
}

/// Everything the calculator needs about a character.
#[derive(Debug, Clone, PartialEq)]
pub struct Input {
    pub constants: Constants,
    pub class: CharacterBase,
    pub level: u32,
    /// The active weapon set and the armour.
    pub items: Vec<ItemInput>,
    /// Stats of the allocated passives, one entry per node.
    pub passives: Vec<Vec<(String, i64)>>,
    /// Debug modifiers and buffs, already as mods.
    pub extra_mods: Vec<Mod>,
    /// The act's elemental resistance penalty, a negative number or 0.
    pub resist_penalty: f64,
    pub skill: SkillProfile,
    pub enemy: Enemy,
    pub conditions: Vec<&'static str>,
}

/// The computed stats. Field names follow PoB2's output names.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Output {
    pub str: f64,
    pub dex: f64,
    pub int: f64,
    pub life: f64,
    pub mana: f64,
    pub spirit: f64,
    pub energy_shield: f64,
    pub armour: f64,
    pub evasion: f64,
    pub ward: f64,
    pub fire_resist: f64,
    pub cold_resist: f64,
    pub lightning_resist: f64,
    pub chaos_resist: f64,
    pub fire_resist_max: f64,
    pub cold_resist_max: f64,
    pub lightning_resist_max: f64,
    pub chaos_resist_max: f64,
    pub block_chance: f64,
    pub block_chance_max: f64,
    pub life_regen: f64,
    pub mana_regen: f64,
    pub es_regen: f64,
    pub es_recharge: f64,
    pub movement_speed_mod: f64,
    pub action_speed_mod: f64,
    pub stun_threshold: f64,
    pub ailment_threshold: f64,
    pub item_rarity: f64,
    pub item_quantity: f64,
    pub light_radius: f64,
    pub charm_slots: f64,
    pub accuracy: f64,
    pub hit_chance: f64,
    pub speed: f64,
    pub crit_chance: f64,
    pub crit_chance_pre_hit: f64,
    pub crit_multiplier: f64,
    pub average_hit: f64,
    /// Stats that had no entry in the stat table, in item order.
    pub not_implemented: Vec<String>,
}

/// The calculator's state between the steps of `calculate`.
pub(crate) struct Env {
    pub db: ModDb,
    pub armour_items: Vec<items::ArmourData>,
    pub weapon_kind: WeaponType,
    pub weapon: WeaponData,
    pub off_hand_armour: Option<items::ArmourData>,
    /// Fraction of the pool per second.
    pub es_recharge_rate: f64,
}

pub(crate) const NO_CFG: Cfg<'static> = Cfg { flags: 0, keyword_flags: 0, slot: None, skill_types: &[] };

/// `calcs.initModDB` and the base mods of `calcs.initEnv`.
fn base_mods(input: &Input, db: &mut ModDb) {
    let c = &input.constants;
    db.multipliers.insert("Level", f64::from(input.level.clamp(1, 100)));
    db.new_mod("Str", Kind::Base, input.class.strength);
    db.new_mod("Dex", Kind::Base, input.class.dexterity);
    db.new_mod("Int", Kind::Base, input.class.intelligence);
    for res in ["FireResistMax", "ColdResistMax", "LightningResistMax", "ChaosResistMax"] {
        db.new_mod(res, Kind::Base, c.max_resistance);
    }
    db.new_mod("BaseBlockChanceMax", Kind::Base, c.max_block);
    db.add(Mod::new("MovementSpeed", Kind::Inc, -30.0).with_tag(Tag::condition("Maimed")));
    db.add(Mod::new("Evasion", Kind::Inc, -15.0).with_tag(Tag::condition("Maimed")));
    db.add(Mod::new("MovementSpeed", Kind::Inc, -30.0).with_tag(Tag::condition("Hindered")));
    db.add(Mod::new("AilmentThreshold", Kind::Base, 50.0).with_tag(Tag::PercentStat { stat: "Life", percent: 1.0 }));
    db.add(Mod::new("MovementSpeed", Kind::Inc, 50.0).with_tag(Tag::condition("Sprinting")));
    db.new_mod("CritChanceCap", Kind::Base, 100.0);
    db.add(Mod::new("Life", Kind::Base, c.life_per_level).with_tag(Tag::Multiplier { var: "Level", div: 1.0, limit: None, base: c.base_life }));
    db.add(Mod::new("Mana", Kind::Base, c.mana_per_level).with_tag(Tag::Multiplier { var: "Level", div: 1.0, limit: None, base: c.base_mana }));
    db.add(
        Mod::new("ManaRegen", Kind::Base, c.mana_regen_per_minute / 60.0 / 100.0)
            .with_tag(Tag::per_stat("Mana", 1.0))
            .with_tag(Tag::Condition { var: "NoInherentManaRegen", neg: true }),
    );
    db.new_mod("Spirit", Kind::Base, 0.0);
    db.new_mod("Evasion", Kind::Base, c.base_evasion);
    db.add(Mod::new("Accuracy", Kind::Base, c.accuracy_per_level).with_tag(Tag::Multiplier { var: "Level", div: 1.0, limit: None, base: -c.accuracy_per_level }));
    db.new_mod("CritMultiplier", Kind::Base, c.base_crit_bonus);
    db.new_mod("FireResist", Kind::Base, input.resist_penalty);
    db.new_mod("ColdResist", Kind::Base, input.resist_penalty);
    db.new_mod("LightningResist", Kind::Base, input.resist_penalty);
    db.new_mod("ChaosResist", Kind::Base, 0.0);
    db.new_mod("CharmLimit", Kind::Base, 0.0);
}

/// `calculateAttributes`: twice, because of the "X attribute higher than Y" conditions.
fn attributes(db: &mut ModDb, out: &mut Output) {
    for _ in 0..2 {
        out.str = round(db.val(&NO_CFG, "Str")).max(0.0);
        out.dex = round(db.val(&NO_CFG, "Dex")).max(0.0);
        out.int = round(db.val(&NO_CFG, "Int")).max(0.0);
        db.stats.insert("Str", out.str);
        db.stats.insert("Dex", out.dex);
        db.stats.insert("Int", out.int);
        let (s, d, i) = (out.str, out.dex, out.int);
        for (name, v) in [
            ("DexHigherThanInt", d > i),
            ("StrHigherThanInt", s > i),
            ("IntHigherThanDex", i > d),
            ("StrHigherThanDex", s > d),
            ("IntHigherThanStr", i > s),
            ("DexHigherThanStr", d > s),
            ("StrHighestAttribute", s >= d && s >= i),
            ("IntHighestAttribute", i >= s && i >= d),
            ("DexHighestAttribute", d >= s && d >= i),
        ] {
            db.conditions.insert(name, v);
        }
    }
}

/// The conditions of `doActorAttribsConditions` that come from the equipment.
fn equipment_conditions(input: &Input, env: &mut Env) {
    let (_, cond, one_hand, melee) = env.weapon_kind.info();
    let db = &mut env.db;
    let off = env.off_hand_armour.as_ref();
    if off.is_some_and(|a| a.shield) {
        db.conditions.insert("UsingShield", true);
    } else if off.is_some_and(|a| a.focus) {
        db.conditions.insert("UsingFocus", true);
    } else if off.is_none() {
        db.conditions.insert("OffHandIsEmpty", true);
    }
    if env.weapon_kind == WeaponType::Unarmed {
        db.conditions.insert("Unarmed", true);
    } else {
        db.conditions.insert(cond, true);
        if melee {
            db.conditions.insert("UsingMeleeWeapon", true);
        }
        db.conditions.insert(if one_hand { "UsingOneHandedWeapon" } else { "UsingTwoHandedWeapon" }, true);
        db.conditions.insert("UsingMartialWeapon", true);
    }
    for item in &input.items {
        let name = match item.slot {
            items::Slot::Helmet => "UsingHelmet",
            items::Slot::BodyArmour => "UsingBody Armour",
            items::Slot::Gloves => "UsingGloves",
            items::Slot::Boots => "UsingBoots",
            _ => continue,
        };
        db.conditions.insert(name, true);
    }
    for c in &input.conditions {
        db.conditions.insert(c, true);
    }
}

/// The full calculation.
pub fn calculate(input: &Input) -> Output {
    let mut out = Output::default();
    let mut db = ModDb::default();
    base_mods(input, &mut db);

    let mut env_weapon = (WeaponType::Unarmed, None);
    let mut armour_items = Vec::new();
    let mut off_hand_armour = None;
    for item in &input.items {
        let built = items::build_item(item);
        for m in built.global {
            db.add(m);
        }
        out.not_implemented.extend(built.not_implemented);
        if let Some((kind, w)) = built.weapon {
            if item.slot == items::Slot::MainHand {
                env_weapon = (kind, Some(w));
            }
        }
        if let Some(a) = built.armour {
            if item.slot == items::Slot::OffHand {
                off_hand_armour = Some(a.clone());
            }
            armour_items.push(a);
        }
    }
    for node in &input.passives {
        let stats: Vec<(&str, i64)> = node.iter().map(|(s, v)| (s.as_str(), *v)).collect();
        let (mods, unknown) = map::mods_for_stats(&stats);
        for m in mods {
            db.add(m);
        }
        out.not_implemented.extend(unknown);
    }
    for m in &input.extra_mods {
        db.add(m.clone());
    }

    let u = input.class.unarmed;
    let weapon = env_weapon.1.unwrap_or_else(|| {
        let mut damage = [(0.0, 0.0); 5];
        damage[0] = (u.physical_min, u.physical_max);
        WeaponData { attack_rate: u.attack_rate, crit_chance: u.crit_chance, reload_time: 0.0, damage }
    });
    let mut env = Env { db, armour_items, weapon_kind: env_weapon.0, weapon, off_hand_armour, es_recharge_rate: input.constants.es_recharge_per_minute / 60.0 / 100.0 };

    equipment_conditions(input, &mut env);
    attributes(&mut env.db, &mut out);
    // Attribute bonuses (`doActorAttribsConditions`).
    env.db.new_mod("Life", Kind::Base, out.str * 2.0);
    env.db.new_mod("Accuracy", Kind::Base, out.dex * defence::ACCURACY_PER_DEX);
    env.db.new_mod("Mana", Kind::Base, out.int * 2.0);

    defence::pools(&mut env, &mut out);
    defence::defence(&mut env, &mut out);
    offence::offence(&mut env, input, &mut out);
    out
}
