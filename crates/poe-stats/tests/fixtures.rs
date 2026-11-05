//! The corpus of `cargo xtask stats-check --write`: characters made from game data and what PoB2
//! computes for them, as plain numbers. Runs without game files or PoB2.

use poe_stats::{CharacterBase, Constants, Enemy, Input, ItemBase, ItemInput, SkillProfile, Slot, WeaponBase, WeaponType, calculate};
use serde_json::Value;

const INTEGER: &[&str] = &[
    "Life", "Mana", "Spirit", "EnergyShield", "Armour", "Evasion", "Ward", "FireResist", "ColdResist", "LightningResist",
    "ChaosResist", "BlockChance", "Str", "Dex", "Int", "Accuracy",
];
const DECIMAL: &[(&str, f64)] = &[
    ("LifeRegen", 0.1),
    ("ManaRegen", 0.1),
    ("EnergyShieldRecharge", 0.1),
    ("AverageHit", 0.01),
    ("Speed", 0.01),
    ("CritChance", 0.01),
    ("CritMultiplier", 0.01),
    ("MovementSpeedMod", 0.001),
    ("StunThreshold", 0.5),
    ("AilmentThreshold", 0.5),
];

fn f(v: &Value) -> f64 {
    v.as_f64().expect("a number")
}

fn numbers<const N: usize>(v: &Value) -> [f64; N] {
    let list: Vec<f64> = v.as_array().expect("a list").iter().map(f).collect();
    list.try_into().expect("the list's length")
}

fn slot(name: &str) -> Slot {
    match name {
        "MainHand" => Slot::MainHand,
        "OffHand" => Slot::OffHand,
        "Helmet" => Slot::Helmet,
        "BodyArmour" => Slot::BodyArmour,
        "Gloves" => Slot::Gloves,
        "Boots" => Slot::Boots,
        "Amulet" => Slot::Amulet,
        "Ring1" => Slot::Ring1,
        "Ring2" => Slot::Ring2,
        "Belt" => Slot::Belt,
        other => panic!("slot {other}"),
    }
}

fn weapon_type(name: &str) -> WeaponType {
    [
        WeaponType::Unarmed,
        WeaponType::Bow,
        WeaponType::Crossbow,
        WeaponType::Claw,
        WeaponType::Dagger,
        WeaponType::Spear,
        WeaponType::Flail,
        WeaponType::Staff,
        WeaponType::Wand,
        WeaponType::OneHandAxe,
        WeaponType::OneHandMace,
        WeaponType::OneHandSword,
        WeaponType::TwoHandAxe,
        WeaponType::TwoHandMace,
        WeaponType::TwoHandSword,
    ]
    .into_iter()
    .find(|w| format!("{w:?}") == name)
    .unwrap_or_else(|| panic!("weapon {name}"))
}

fn stats(v: &Value) -> Vec<(String, i64)> {
    v.as_array().expect("stats").iter().map(|s| (s[0].as_str().expect("stat").to_owned(), s[1].as_i64().expect("value"))).collect()
}

fn item(v: &Value) -> ItemInput {
    let b = &v["base"];
    let base = if let Some(a) = b.get("armour") {
        ItemBase::Armour {
            armour: f(&a[0]),
            evasion: f(&a[1]),
            energy_shield: f(&a[2]),
            ward: f(&a[3]),
            block: a[4].as_f64(),
            movement_penalty: f(&a[5]),
            shield: a[6].as_bool().expect("shield"),
            focus: a[7].as_bool().expect("focus"),
        }
    } else if let Some(w) = b.get("weapon") {
        ItemBase::Weapon(WeaponBase {
            kind: weapon_type(w[0].as_str().expect("kind")),
            physical_min: f(&w[1]),
            physical_max: f(&w[2]),
            attack_rate: f(&w[3]),
            crit_chance: f(&w[4]),
            reload_time: f(&w[5]),
        })
    } else {
        ItemBase::Other { spirit: None, charm_limit: None }
    };
    ItemInput { slot: slot(v["slot"].as_str().expect("slot")), base, quality: f(&v["quality"]), stats: stats(&v["stats"]) }
}

fn input(fx: &Value) -> Input {
    let [life_per_level, mana_per_level, accuracy_per_level, base_evasion, base_crit_bonus, max_resistance, max_block, mana_regen_per_minute, es_recharge_per_minute, base_life, base_mana] = numbers(&fx["constants"]);
    let [strength, dexterity, intelligence, physical_min, physical_max, attack_rate, crit_chance] = numbers(&fx["class"]);
    let [evasion, armour, physical_reduction, max_physical_reduction, lightning, cold, fire, chaos] = numbers(&fx["enemy"]);
    let items: Vec<ItemInput> = fx["items"].as_array().expect("items").iter().map(item).collect();
    let kind = items
        .iter()
        .find_map(|i| match (&i.slot, &i.base) {
            (Slot::MainHand, ItemBase::Weapon(w)) => Some(w.kind),
            _ => None,
        })
        .unwrap_or(WeaponType::Unarmed);
    let mut skill = SkillProfile::default_attack(kind, f(&fx["skill_multiplier"]));
    skill.base_bolts = f(&fx["skill_bolts"]);
    Input {
        constants: Constants { life_per_level, mana_per_level, accuracy_per_level, base_evasion, base_crit_bonus, max_resistance, max_block, mana_regen_per_minute, es_recharge_per_minute, base_life, base_mana },
        class: CharacterBase {
            strength,
            dexterity,
            intelligence,
            unarmed: WeaponBase { kind: WeaponType::Unarmed, physical_min, physical_max, attack_rate, crit_chance, reload_time: 0.0 },
        },
        level: fx["level"].as_u64().expect("level") as u32,
        items,
        passives: fx["passives"].as_array().expect("passives").iter().map(stats).collect(),
        extra_mods: Vec::new(),
        resist_penalty: f(&fx["resist_penalty"]),
        skill,
        enemy: Enemy { evasion, armour, physical_reduction, max_physical_reduction, resist: [lightning, cold, fire, chaos] },
        conditions: Vec::new(),
    }
}

#[test]
fn the_calculator_matches_pob2_on_the_corpus() {
    let fixtures: Vec<Value> = serde_json::from_str(include_str!("fixtures.json")).expect("fixtures.json");
    assert!(fixtures.len() >= 50);
    let mut classes = std::collections::HashSet::new();
    let mut failures = Vec::new();
    for fx in &fixtures {
        classes.insert(fx["name"].as_str().expect("name").split('-').next().expect("class"));
        let got = calculate(&input(fx));
        for (key, tolerance) in INTEGER.iter().map(|k| (*k, 0.0)).chain(DECIMAL.iter().copied()) {
            let Some(want) = fx["expected"][key].as_f64() else { continue };
            let have = got.by_pob_name(key).unwrap_or(f64::NAN);
            let close = (have - want).abs() <= tolerance + 1e-9;
            if !close {
                failures.push(format!("{} {key}: {have} vs PoB2 {want}", fx["name"]));
            }
        }
    }
    assert_eq!(classes.len(), 8, "every class");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
