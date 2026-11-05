//! Item-local rules: PoB2's `ItemClass:BuildModListForSlotNum`. Local mods of an armour piece or a
//! weapon are consumed here (`calcLocal`) and change the item's own numbers; the others go on to
//! the character.

use crate::{
    Mod, round, round_to,
    map,
    moddb::{Kind, flag},
};

/// Equipment slots of the calculator (PoB2's names in [`Slot::pob_name`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Slot {
    MainHand,
    OffHand,
    Helmet,
    BodyArmour,
    Gloves,
    Boots,
    Amulet,
    Ring1,
    Ring2,
    Belt,
}

impl Slot {
    pub fn pob_name(self) -> &'static str {
        match self {
            Self::MainHand => "Weapon 1",
            Self::OffHand => "Weapon 2",
            Self::Helmet => "Helmet",
            Self::BodyArmour => "Body Armour",
            Self::Gloves => "Gloves",
            Self::Boots => "Boots",
            Self::Amulet => "Amulet",
            Self::Ring1 => "Ring 1",
            Self::Ring2 => "Ring 2",
            Self::Belt => "Belt",
        }
    }
}

/// `data.weaponTypeInfo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WeaponType {
    Unarmed,
    Bow,
    Crossbow,
    Claw,
    Dagger,
    Spear,
    Flail,
    Staff,
    Wand,
    OneHandAxe,
    OneHandMace,
    OneHandSword,
    TwoHandAxe,
    TwoHandMace,
    TwoHandSword,
    Talisman,
}

impl WeaponType {
    /// `(flag, condition name, one handed, melee)`.
    pub fn info(self) -> (u64, &'static str, bool, bool) {
        match self {
            Self::Unarmed => (flag::UNARMED, "UsingUnarmed", true, true),
            Self::Bow => (flag::BOW, "UsingBow", false, false),
            Self::Crossbow => (flag::CROSSBOW, "UsingCrossbow", false, false),
            Self::Claw => (flag::CLAW, "UsingClaw", true, true),
            Self::Dagger => (flag::DAGGER, "UsingDagger", true, true),
            Self::Spear => (flag::SPEAR, "UsingSpear", true, true),
            Self::Flail => (flag::FLAIL, "UsingFlail", true, true),
            Self::Staff => (flag::STAFF, "UsingStaff", false, true),
            Self::Wand => (flag::WAND, "UsingWand", true, false),
            Self::OneHandAxe => (flag::AXE, "UsingAxe", true, true),
            Self::OneHandMace => (flag::MACE, "UsingMace", true, true),
            Self::OneHandSword => (flag::SWORD, "UsingSword", true, true),
            Self::TwoHandAxe => (flag::AXE, "UsingAxe", false, true),
            Self::TwoHandMace => (flag::MACE, "UsingMace", false, true),
            Self::TwoHandSword => (flag::SWORD, "UsingSword", false, true),
            Self::Talisman => (flag::TALISMAN, "UsingTalisman", false, true),
        }
    }

    /// PoB2's `getWeaponFlags`.
    pub fn mod_flags(self) -> u64 {
        let (f, _, one_hand, melee) = self.info();
        if self == Self::Unarmed {
            return f;
        }
        f | flag::WEAPON
            | if one_hand { flag::WEAPON_1H } else { flag::WEAPON_2H }
            | if melee { flag::WEAPON_MELEE } else { flag::WEAPON_RANGED }
    }
}

/// A weapon's base numbers (`WeaponTypes.dat`), or the class's unarmed numbers (`Characters.dat`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WeaponBase {
    pub kind: WeaponType,
    pub physical_min: f64,
    pub physical_max: f64,
    /// Attacks per second.
    pub attack_rate: f64,
    /// Percent.
    pub crit_chance: f64,
    /// Seconds; 0 for anything but a crossbow.
    pub reload_time: f64,
}

/// An item base's numbers.
#[derive(Debug, Clone, PartialEq)]
pub enum ItemBase {
    /// `ArmourTypes.dat` and `ShieldTypes.dat`.
    Armour {
        armour: f64,
        evasion: f64,
        energy_shield: f64,
        ward: f64,
        /// Shields only.
        block: Option<f64>,
        /// A fraction (`0.05` slows by 5%).
        movement_penalty: f64,
        shield: bool,
        focus: bool,
    },
    Weapon(WeaponBase),
    /// Rings, amulets, belts, sceptres: `spirit` when the base grants it, `charm_limit` for belts.
    Other { spirit: Option<f64>, charm_limit: Option<f64> },
}

/// An equipped item.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemInput {
    pub slot: Slot,
    pub base: ItemBase,
    pub quality: f64,
    /// The stats of all its mods (implicit, explicit, enchant), summed by stat id.
    pub stats: Vec<(String, i64)>,
}

/// A weapon after its local mods (`weaponData`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WeaponData {
    pub attack_rate: f64,
    pub crit_chance: f64,
    /// Seconds; 0 for anything but a crossbow.
    pub reload_time: f64,
    /// `[Physical, Lightning, Cold, Fire, Chaos]`, min and max.
    pub damage: [(f64, f64); 5],
}

pub const DAMAGE_TYPES: [&str; 5] = ["Physical", "Lightning", "Cold", "Fire", "Chaos"];

/// An armour piece after its local mods (`armourData`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ArmourData {
    pub armour: f64,
    pub evasion: f64,
    pub energy_shield: f64,
    pub ward: f64,
    pub block: f64,
    pub shield: bool,
    pub focus: bool,
}

/// An item after [`build_item`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BuiltItem {
    pub weapon: Option<(WeaponType, WeaponData)>,
    pub armour: Option<ArmourData>,
    /// Mods that apply to the character.
    pub global: Vec<Mod>,
    /// Stats with no entry in the stat table.
    pub not_implemented: Vec<String>,
}

/// `calcLocal`: sums and removes the untagged mods of this name, type and flags.
fn take(mods: &mut Vec<Mod>, name: &str, kind: Kind, flags: u64) -> f64 {
    let mut total = 0.0;
    mods.retain(|m| {
        if m.name == name && m.kind == kind && m.flags == flags && m.keyword_flags == 0 && m.tags.is_empty() {
            total += m.value;
            false
        } else {
            true
        }
    });
    total
}

pub fn build_item(item: &ItemInput) -> BuiltItem {
    let stats: Vec<(&str, i64)> = item.stats.iter().map(|(s, v)| (s.as_str(), *v)).collect();
    let (mut mods, not_implemented) = map::mods_for_stats(&stats);
    let mut built = BuiltItem::default();
    let q = item.quality;
    match &item.base {
        ItemBase::Weapon(base) => {
            let speed_inc = take(&mut mods, "Speed", Kind::Inc, flag::ATTACK);
            let attack_rate = round_to(base.attack_rate * (1.0 + speed_inc / 100.0), 2);
            let reload_inc = take(&mut mods, "ReloadSpeed", Kind::Inc, flag::ATTACK) + speed_inc;
            let reload_time = round_to(base.reload_time / (1.0 + reload_inc / 100.0), 2);
            let local_ele = take(&mut mods, "LocalElementalDamage", Kind::Inc, 0);
            let mut damage = [(0.0, 0.0); 5];
            for (i, ty) in DAMAGE_TYPES.iter().enumerate() {
                let (bmin, bmax) = if i == 0 { (base.physical_min, base.physical_max) } else { (0.0, 0.0) };
                let mut min = bmin + take(&mut mods, &format!("{ty}Min"), Kind::Base, 0);
                let mut max = bmax + take(&mut mods, &format!("{ty}Max"), Kind::Base, 0);
                if i == 0 {
                    let inc = take(&mut mods, "PhysicalDamage", Kind::Inc, 0);
                    min = round(min * (1.0 + inc / 100.0) * (1.0 + q / 100.0));
                    max = round(max * (1.0 + inc / 100.0) * (1.0 + q / 100.0));
                } else {
                    let inc = take(&mut mods, &format!("Local{ty}Damage"), Kind::Inc, 0) + local_ele;
                    min = round(min * (1.0 + inc / 100.0));
                    max = round(max * (1.0 + inc / 100.0));
                }
                damage[i] = (min, max);
            }
            let crit_base = take(&mut mods, "CritChance", Kind::Base, 0);
            let crit_inc = take(&mut mods, "CritChance", Kind::Inc, 0);
            let crit_chance = round_to((base.crit_chance + crit_base) * (1.0 + crit_inc / 100.0), 2);
            built.weapon = Some((base.kind, WeaponData { attack_rate, crit_chance, reload_time, damage }));
        }
        ItemBase::Armour { armour, evasion, energy_shield, ward, block, movement_penalty, shield, focus } => {
            let a_base = take(&mut mods, "Armour", Kind::Base, 0) + armour;
            let ae_base = take(&mut mods, "ArmourAndEvasion", Kind::Base, 0);
            let ev_base = take(&mut mods, "Evasion", Kind::Base, 0) + evasion;
            let ees_base = take(&mut mods, "EvasionAndEnergyShield", Kind::Base, 0);
            let es_base = take(&mut mods, "EnergyShield", Kind::Base, 0) + energy_shield;
            let aes_base = take(&mut mods, "ArmourAndEnergyShield", Kind::Base, 0);
            let w_base = take(&mut mods, "Ward", Kind::Base, 0) + ward;
            let a_inc = take(&mut mods, "Armour", Kind::Inc, 0);
            let ae_inc = take(&mut mods, "ArmourAndEvasion", Kind::Inc, 0);
            let ev_inc = take(&mut mods, "Evasion", Kind::Inc, 0);
            let ees_inc = take(&mut mods, "EvasionAndEnergyShield", Kind::Inc, 0);
            let es_inc = take(&mut mods, "EnergyShield", Kind::Inc, 0);
            let w_inc = take(&mut mods, "Ward", Kind::Inc, 0);
            let aes_inc = take(&mut mods, "ArmourAndEnergyShield", Kind::Inc, 0);
            let def_inc = take(&mut mods, "Defences", Kind::Inc, 0);
            let qf = 1.0 + q / 100.0;
            let mut data = ArmourData {
                armour: round((a_base + ae_base + aes_base) * (1.0 + (a_inc + ae_inc + aes_inc + def_inc) / 100.0) * qf),
                evasion: round((ev_base + ae_base + ees_base) * (1.0 + (ev_inc + ae_inc + ees_inc + def_inc) / 100.0) * qf),
                energy_shield: round((es_base + ees_base + aes_base) * (1.0 + (es_inc + aes_inc + ees_inc + def_inc) / 100.0) * qf),
                ward: round(w_base * (1.0 + (w_inc + def_inc) / 100.0) * qf),
                block: 0.0,
                shield: *shield,
                focus: *focus,
            };
            if let Some(b) = block {
                let block_base = take(&mut mods, "BlockChance", Kind::Base, 0);
                let block_inc = take(&mut mods, "BlockChance", Kind::Inc, 0);
                data.block = ((b + block_base) * (1.0 + block_inc / 100.0)).floor();
            }
            if *movement_penalty != 0.0 {
                mods.push(Mod::new("MovementSpeed", Kind::Base, -movement_penalty));
            }
            built.armour = Some(data);
        }
        ItemBase::Other { spirit, charm_limit } => {
            if let Some(s) = spirit {
                let base = s + take(&mut mods, "Spirit", Kind::Base, 0);
                let inc = take(&mut mods, "Spirit", Kind::Inc, 0);
                mods.push(Mod::new("Spirit", Kind::Base, round(base * (1.0 + inc / 100.0))));
            }
            if let Some(c) = charm_limit {
                let total = c + take(&mut mods, "CharmLimit", Kind::Base, 0);
                mods.push(Mod::new("CharmLimit", Kind::Base, total));
            }
        }
    }
    built.global = mods;
    built.not_implemented = not_implemented;
    built
}

#[cfg(test)]
mod tests {
    use super::*;

    fn armour(base: f64, quality: f64, stats: &[(&str, i64)]) -> BuiltItem {
        build_item(&ItemInput {
            slot: Slot::Helmet,
            base: ItemBase::Armour {
                armour: base,
                evasion: 0.0,
                energy_shield: 0.0,
                ward: 0.0,
                block: None,
                movement_penalty: 0.0,
                shield: false,
                focus: false,
            },
            quality,
            stats: stats.iter().map(|(s, v)| ((*s).to_owned(), *v)).collect(),
        })
    }

    #[test]
    fn armour_local_flat_inc_and_quality_stack_as_pob() {
        let b = armour(100.0, 20.0, &[("local_base_physical_damage_reduction_rating", 50), ("local_physical_damage_reduction_rating_+%", 10)]);
        // (100 + 50) * 1.10 * 1.20
        assert_eq!(b.armour.unwrap().armour, 198.0);
        assert!(b.global.is_empty());
    }

    #[test]
    fn global_stat_on_armour_stays_global() {
        let b = armour(10.0, 0.0, &[("base_maximum_life", 40)]);
        assert_eq!(b.global.len(), 1);
        assert_eq!(b.global[0].name, "Life");
    }

    #[test]
    fn unknown_stat_is_reported() {
        let b = armour(10.0, 0.0, &[("some_unique_effect", 1)]);
        assert_eq!(b.not_implemented, ["some_unique_effect"]);
    }

    #[test]
    fn weapon_local_damage() {
        let b = build_item(&ItemInput {
            slot: Slot::MainHand,
            base: ItemBase::Weapon(WeaponBase { kind: WeaponType::Bow, physical_min: 6.0, physical_max: 9.0, attack_rate: 1.2, crit_chance: 5.0, reload_time: 0.0 }),
            quality: 10.0,
            stats: vec![
                ("local_physical_damage_+%".into(), 50),
                ("local_attack_speed_+%".into(), 10),
                ("local_minimum_added_fire_damage".into(), 3),
                ("local_maximum_added_fire_damage".into(), 7),
            ],
        });
        let (kind, w) = b.weapon.unwrap();
        assert_eq!(kind, WeaponType::Bow);
        assert_eq!(w.attack_rate, 1.32);
        // 6 * 1.5 * 1.1 = 9.9, 9 * 1.5 * 1.1 = 14.85
        assert_eq!(w.damage[0], (10.0, 15.0));
        assert_eq!(w.damage[3], (3.0, 7.0));
        assert!(b.global.is_empty());
    }
}
