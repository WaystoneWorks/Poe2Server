//! Typed views over the tables the servers use. Column names follow the community schema.

use crate::{GameData, Result};

/// A `CharacterStartStates` row resolved to its class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartState {
    pub id: String,
    /// `Characters.IntegerId` of the class.
    pub class_id: u32,
    /// `Characters.Name`, e.g. "Ranger".
    pub class_name: String,
    pub level: u32,
    /// `WorldAreas.Id` of the start state's hub area, if it has one.
    pub hub_area: Option<String>,
}

impl StartState {
    /// Looks up a start state by its id (e.g. `"DexFourDefault"`).
    pub fn find(data: &GameData, id: &str) -> Result<Option<Self>> {
        let states = data.table("CharacterStartStates")?;
        let Some(row) = states.find_by_str("Id", id)? else {
            return Ok(None);
        };
        let level = row.int("Level")? as u32;
        let hub_area = match row.row_ref("HubArea")? {
            Some(i) => data
                .table("WorldAreas")?
                .row(i)
                .map(|r| r.str("Id"))
                .transpose()?
                .filter(|id| !id.is_empty()),
            None => None,
        };
        let Some(character) = row.row_ref("Character")? else {
            return Ok(None);
        };
        let characters = data.table("Characters")?;
        let Some(class) = characters.row(character) else {
            return Ok(None);
        };
        Ok(Some(Self {
            id: id.to_owned(),
            class_id: class.int("IntegerId")? as u32,
            class_name: class.str("Name")?,
            level,
            hub_area,
        }))
    }
}

/// A `WorldAreas` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorldArea {
    pub id: String,
    pub name: String,
    pub act: i64,
    pub level: i64,
    pub is_town: bool,
    pub is_hideout: bool,
    /// 16-bit area hash the client uses to reference areas compactly.
    pub hash16: u16,
}

impl WorldArea {
    /// Towns and hideouts always use layout seed 1 (every one of them in live client logs);
    /// other areas get a random seed per instance.
    pub fn fixed_seed(&self) -> Option<u32> {
        (self.is_town || self.is_hideout).then_some(1)
    }

    /// The town a player of this area is sent back to: the `IsTown` area of the same act whose
    /// id starts like this area's (`G1_2` -> `G1_town`, `P2_3` -> `P2_Town`; act 6 has three
    /// towns), else the first town of the act. The login scene is never one. `None` when the act
    /// has no town.
    pub fn town(&self, data: &GameData) -> Result<Option<Self>> {
        let prefix = self.id.split('_').next().unwrap_or_default().to_ascii_lowercase();
        let mut first = None;
        for row in data.table("WorldAreas")?.rows() {
            let id = row.str("Id")?;
            if !row.bool("IsTown")? || row.int("Act")? != self.act || id == "G_login" {
                continue;
            }
            if id.split('_').next().unwrap_or_default().to_ascii_lowercase() == prefix {
                return Self::find(data, &id);
            }
            first.get_or_insert(id);
        }
        first.map(|id| Self::find(data, &id)).transpose().map(Option::flatten)
    }

    /// Every area, in table order.
    pub fn all(data: &GameData) -> Result<Vec<Self>> {
        let mut out = Vec::new();
        for row in data.table("WorldAreas")?.rows() {
            if let Some(area) = Self::find(data, &row.str("Id")?)? {
                out.push(area);
            }
        }
        Ok(out)
    }

    pub fn find(data: &GameData, id: &str) -> Result<Option<Self>> {
        let areas = data.table("WorldAreas")?;
        let Some(row) = areas.find_by_str("Id", id)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            id: id.to_owned(),
            name: row.str("Name")?,
            act: row.int("Act")?,
            level: row.int("AreaLevel")?,
            is_town: row.bool("IsTown")?,
            is_hideout: row.bool("IsHideout")?,
            hash16: row.int("HASH16")? as u16,
        }))
    }
}

/// A `Characters` row (a playable class).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CharacterClass {
    /// `Characters.AOFile`: the class's animated object (model).
    pub ao_file: String,
    pub base_strength: i64,
    pub base_dexterity: i64,
    pub base_intelligence: i64,
    /// Metadata path of the class's object type, e.g. `Metadata/Characters/Dex/DexFour`.
    pub metadata_id: String,
    pub name: String,
    pub integer_id: u32,
    pub base_max_life: i64,
    pub base_max_mana: i64,
}

impl CharacterClass {
    /// Looks up a class by `Characters.IntegerId` (the id stored with each character).
    pub fn by_integer_id(data: &GameData, integer_id: u32) -> Result<Option<Self>> {
        let characters = data.table("Characters")?;
        for i in 0..characters.len() {
            let Some(row) = characters.row(i) else {
                continue;
            };
            if row.int("IntegerId")? as u32 == integer_id {
                return Ok(Some(Self {
                    metadata_id: row.str("Id")?,
                    name: row.str("Name")?,
                    integer_id,
                    base_max_life: row.int("BaseMaxLife")?,
                    base_max_mana: row.int("BaseMaxMana")?,
                    ao_file: row.str("AOFile")?,
                    base_strength: row.int("BaseStrength")?,
                    base_dexterity: row.int("BaseDexterity")?,
                    base_intelligence: row.int("BaseIntelligence")?,
                }));
            }
        }
        Ok(None)
    }

    /// Object type key the client uses for this class's player entity.
    pub fn object_type_key(&self) -> u32 {
        crate::hash::object_type_key(&self.metadata_id)
    }
}

/// An `Inventories` row: a kind of inventory (main bag, an equipment slot, ...). The row index
/// is the slot index the client uses (inventory +0x144; 121 rows in 0.5.5e.hotfix1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventorySlot {
    pub index: u8,
    pub id: String,
    pub id_key: i64,
}

impl InventorySlot {
    pub fn all(data: &GameData) -> Result<Vec<Self>> {
        let table = data.table("Inventories")?;
        (0..table.len())
            .filter_map(|i| table.row(i).map(|r| (i, r)))
            .map(|(i, row)| {
                Ok(Self {
                    index: i as u8,
                    id: row.str("Id")?,
                    id_key: row.int("InventoryIdKey")?,
                })
            })
            .collect()
    }
}

/// Index of a `LeagueFlag` row by id. The client identifies a character's league flag
/// (banner and icon) by row + 1; 0 or 1 select the generic event banner.
pub fn league_flag_index(data: &GameData, id: &str) -> Result<Option<u32>> {
    let table = data.table("LeagueFlag")?;
    for i in 0..table.len() {
        if let Some(row) = table.row(i)
            && row.str("Id")? == id
        {
            return Ok(Some(i as u32));
        }
    }
    Ok(None)
}

/// A `LeagueFlag` row: a league's banner art. The client takes it by row + 1 from
/// CharacterEntry +0x4d (select screen) and LeagueEntry +0xc2 (creation banner,
/// `0x140e77810`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeagueFlagRow {
    pub row: u32,
    pub id: String,
    /// Small icon (`Art/2DArt/UIImages/Common/League...`), may be empty.
    pub image: String,
    /// Login-screen banner, may be empty.
    pub banner: String,
    /// Creation-screen banner icon, may be empty.
    pub banner_art: String,
    /// League logo (`.dds`), may be empty.
    pub logo_art: String,
    pub hardcore: bool,
    pub ssf: bool,
    pub ruthless: bool,
}

impl LeagueFlagRow {
    /// Every row, in table order.
    pub fn all(data: &GameData) -> Result<Vec<Self>> {
        let table = data.table("LeagueFlag")?;
        table
            .rows()
            .enumerate()
            .map(|(i, r)| {
                Ok(Self {
                    row: i as u32,
                    id: r.str("Id")?,
                    image: r.str("Image")?,
                    banner: r.str("Banner")?,
                    banner_art: r.str("BannerArt")?,
                    logo_art: r.str("LogoArt")?,
                    hardcore: r.bool("IsHC")?,
                    ssf: r.bool("IsSSF")?,
                    ruthless: r.bool("IsRuthless")?,
                })
            })
            .collect()
    }

    /// The flag of the same set for a Hardcore and/or SSF variant, after the table's naming:
    /// `<Set>Standard` / `<Set>Hardcore` / `<Set>StandardSSF` / `<Set>HardcoreSSF`, and
    /// `StandardFlag` / `HardcoreFlag` / `StandardSSF` / `HardcoreSSF` for Standard. `None`
    /// when the set has no such row.
    pub fn variant_of<'a>(rows: &'a [Self], base: &str, hardcore: bool, ssf: bool) -> Option<&'a Self> {
        let want = |id: &str| rows.iter().find(|r| r.id == id);
        if !hardcore && !ssf {
            return want(base);
        }
        let suffix = match (hardcore, ssf) {
            (true, false) => "Hardcore",
            (false, true) => "StandardSSF",
            _ => "HardcoreSSF",
        };
        let id = if base == "StandardFlag" {
            if suffix == "Hardcore" { "HardcoreFlag".to_owned() } else { suffix.to_owned() }
        } else {
            format!("{}{suffix}", base.strip_suffix("Standard")?)
        };
        want(&id).filter(|r| r.hardcore == hardcore && r.ssf == ssf)
    }
}

/// League mechanics (`CoreLeagues.Id`: Breach, Delirium, Ritual, ...), for the league editor.
/// Stored with a league only; none is implemented.
pub fn league_mechanics(data: &GameData) -> Result<Vec<String>> {
    let table = data.table("CoreLeagues")?;
    table.rows().map(|r| r.str("Id")).collect()
}

/// Network key of a stat: `Stats.dat` row + 1 (the client queries e.g. `actor_scale_+%`,
/// row 1913, as 1914).
pub fn stat_key(data: &GameData, id: &str) -> Result<Option<u32>> {
    let table = data.table("Stats")?;
    Ok(table.find_index_by_str("Id", id)?.map(|i| i as u32 + 1))
}

/// 16-bit key of an `ActionTypes` row (its second column) by id, e.g. `Move` = 10505. The
/// client names actions by this key; basic-action skills have the id `0x20000000 | key`.
pub fn action_type_key(data: &GameData, id: &str) -> Result<Option<u16>> {
    let table = data.table("ActionTypes")?;
    let Some(row) = table.find_by_str("Id", id)? else {
        return Ok(None);
    };
    Ok(row.get_index(1)?.as_int().map(|v| v as u16))
}

/// A granted effect (skill) at a level, as the client references it in an actor's skill list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantedSkill {
    /// `GrantedEffects` row.
    pub effect: usize,
    /// `GrantedEffectsPerLevel` row for the level.
    pub effect_level_row: u32,
    /// `GrantedEffectStatSetsPerLevel` rows: the base stat set, then the additional ones.
    pub stat_set_rows: Vec<u32>,
    /// `ActionTypes` key of the skill's action (through `ActiveSkills.ActionType`).
    pub action_type: Option<u16>,
    /// Constant stats of the base stat set (`GrantedEffectStatSets.ConstantStats` ids and
    /// values), e.g. `dodge_roll_base_travel_distance`.
    pub constant_stats: Vec<(String, i64)>,
    /// Implicit (flag) stats of the base stat set (`GrantedEffectStatSets.ImplicitStats`).
    pub implicit_stats: Vec<String>,
}

impl GrantedSkill {
    pub fn constant_stat(&self, id: &str) -> Option<i64> {
        self.constant_stats
            .iter()
            .find(|(k, _)| k == id)
            .map(|&(_, v)| v)
    }
}

impl GrantedSkill {
    /// Looks up a granted effect by id (e.g. `"DodgeRollPlayer"`) at a level.
    pub fn find(data: &GameData, id: &str, level: i64) -> Result<Option<Self>> {
        let effects = data.table("GrantedEffects")?;
        let Some(effect) = effects.find_index_by_str("Id", id)? else {
            return Ok(None);
        };
        let row = effects.row(effect).expect("row from index");
        let per_level = data.table("GrantedEffectsPerLevel")?;
        let Some(effect_level_row) = per_level.rows().position(|r| {
            r.row_ref("GrantedEffect").ok().flatten() == Some(effect)
                && r.int("Level").ok() == Some(level)
        }) else {
            return Ok(None);
        };
        let mut stat_sets: Vec<usize> = row.row_ref("StatSet")?.into_iter().collect();
        let mut constant_stats = Vec::new();
        let mut implicit_stats = Vec::new();
        if let Some(&base) = stat_sets.first() {
            let sets = data.table("GrantedEffectStatSets")?;
            let stats = data.table("Stats")?;
            if let Some(set) = sets.row(base) {
                let ids = set.get("ConstantStats")?;
                let values = set.get("ConstantStatsValues")?;
                if let (Some(ids), Some(values)) = (ids.as_list(), values.as_list()) {
                    for (id, value) in ids.iter().zip(values) {
                        if let (Some(row), Some(value)) =
                            (id.as_row().and_then(|i| stats.row(i)), value.as_int())
                        {
                            constant_stats.push((row.str("Id")?, value));
                        }
                    }
                }
                if let Some(ids) = set.get("ImplicitStats")?.as_list() {
                    for row in ids
                        .iter()
                        .filter_map(|v| v.as_row().and_then(|i| stats.row(i)))
                    {
                        implicit_stats.push(row.str("Id")?);
                    }
                }
            }
        }
        if let Some(list) = row.get("AdditionalStatSets")?.as_list() {
            stat_sets.extend(list.iter().filter_map(|v| v.as_row()));
        }
        let sets_per_level = data.table("GrantedEffectStatSetsPerLevel")?;
        let mut stat_set_rows = Vec::new();
        for set in stat_sets {
            let Some(i) = sets_per_level.rows().position(|r| {
                r.row_ref("StatSet").ok().flatten() == Some(set)
                    && r.int("GemLevel").ok() == Some(level)
            }) else {
                return Ok(None);
            };
            stat_set_rows.push(i as u32);
        }
        let action_type = match row.row_ref("ActiveSkill")? {
            Some(skill) => match data
                .table("ActiveSkills")?
                .row(skill)
                .map(|r| r.row_ref("ActionType"))
                .transpose()?
                .flatten()
            {
                Some(t) => data
                    .table("ActionTypes")?
                    .row(t)
                    .map(|r| r.get_index(1))
                    .transpose()?
                    .and_then(|v| v.as_int())
                    .map(|v| v as u16),
                None => None,
            },
            None => None,
        };
        Ok(Some(Self {
            effect,
            effect_level_row: effect_level_row as u32,
            stat_set_rows,
            action_type,
            constant_stats,
            implicit_stats,
        }))
    }
}

/// A row a player can name: its id and its display name (empty when it has none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedRow {
    pub id: String,
    pub display: String,
}

/// Rows of a game table for command arguments and completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NameList {
    Area,
    Waypoint,
    Class,
    Ascendancy,
    ItemBase,
    Unique,
    Mod,
    Stat,
    Currency,
    Gem,
    Monster,
    Rarity,
    DamageType,
}

impl NameList {
    /// Reads the list from the game data.
    pub fn rows(self, data: &GameData) -> Result<Vec<NamedRow>> {
        match self {
            Self::Area => id_name(data, "WorldAreas", "Name"),
            Self::Waypoint => {
                let mut out = Vec::new();
                for row in data.table("WorldAreas")?.rows() {
                    if row.bool("HasWaypoint")? {
                        out.push(NamedRow { id: row.str("Id")?, display: row.str("Name")? });
                    }
                }
                Ok(out)
            }
            Self::Class => id_name(data, "Characters", "Name"),
            Self::Ascendancy => id_name(data, "Ascendancy", "Name"),
            Self::ItemBase => id_name(data, "BaseItemTypes", "Name"),
            Self::Mod => id_name(data, "Mods", "Name"),
            Self::Monster => id_name(data, "MonsterVarieties", "Name"),
            Self::Stat => id_name(data, "Stats", "Id"),
            Self::Rarity => id_name(data, "Rarity", "Id"),
            // The game has no damage type table; its per-type damage-over-time context flags
            // name each type once (`PhysicalDot`, `FireDot`, ...).
            Self::DamageType => {
                let mut out = Vec::new();
                for row in data.table("VirtualStatContextFlags")?.rows() {
                    if let Some(kind) = row.str("Id")?.strip_suffix("Dot") {
                        out.push(NamedRow { id: kind.to_owned(), display: String::new() });
                    }
                }
                Ok(out)
            }
            Self::Currency => through_base(data, "CurrencyItems"),
            Self::Gem => through_base(data, "SkillGems"),
            Self::Unique => {
                let words = data.table("Words")?;
                let mut out: Vec<NamedRow> = Vec::new();
                for row in data.table("UniqueStashLayout")?.rows() {
                    let Some(word) = row.row_ref("WordsKey")?.and_then(|i| words.row(i)) else {
                        continue;
                    };
                    let id = word.str("Text")?;
                    if !out.iter().any(|n| n.id == id) {
                        out.push(NamedRow { display: id.clone(), id });
                    }
                }
                Ok(out)
            }
        }
    }
}

fn id_name(data: &GameData, table: &str, name: &str) -> Result<Vec<NamedRow>> {
    let mut out = Vec::new();
    for row in data.table(table)?.rows() {
        let id = row.str("Id")?;
        let display = if name == "Id" { String::new() } else { row.str(name)? };
        // Rows the game marks as unused ("[DNT-UNUSED] Bait Fisher").
        if !display.starts_with("[DNT") {
            out.push(NamedRow { id, display });
        }
    }
    Ok(out)
}

/// A table whose rows are named by their `BaseItemType` row.
fn through_base(data: &GameData, table: &str) -> Result<Vec<NamedRow>> {
    let bases = data.table("BaseItemTypes")?;
    let mut out = Vec::new();
    for row in data.table(table)?.rows() {
        if let Some(base) = row.row_ref("BaseItemType")?.and_then(|i| bases.row(i)) {
            out.push(NamedRow { id: base.str("Id")?, display: base.str("Name")? });
        }
    }
    Ok(out)
}
