//! The object stage: the travel objects of a generated area (fog gates and other area
//! transitions, waypoints, checkpoints, healing wells), with where each gate leads.
//!
//! The client shows these only when the server sends them (docs/protocol/entities.md in the hub).
//! Where a gate leads is the server's own choice, so the rules are inferred from the official
//! capture (docs/re/world-travel.md):
//!
//! 1. A room's gates, in placement order, take the room's `entranceN` tags
//!    (`WorldAreas.Connections[N-1]`), then its `teleportN` tags (the room tagged `telepadN` of the
//!    same layout).
//! 2. A gate left over takes the next connection no tag names (`G1_town`: one room tagged
//!    `entrance1`, four gates, connections `G1_1, G1_2, G1_2, G1_2, G2_1`; the capture shows the
//!    last three gates leading to `G1_2` with index 0, 1, 2). A room with tags and no connection
//!    left repeats its last target.
//! 3. Gates with no tag and no connection left are paired with each other in placement order,
//!    as teleports within the instance. A single gate left over leads to the first connection.

use std::collections::{BTreeMap, BTreeSet};

use poe_data::{GameData, template::ObjectTemplate};

use crate::{
    area::Generated,
    doodads::{Doodad, Side},
    generate::{GenError, TaggedRoom},
};

/// Cells per tile.
const TILE: i32 = 23;

/// The fog gate's type: its blob layout is the captured one (docs/re/world-travel.md). A gate type
/// with other components is sent as this type with its own art.
const FOG_GATE: &str = "Metadata/MiscellaneousObjects/AreaTransition_Animate";
const WAYPOINT: &str = "Metadata/MiscellaneousObjects/Waypoint";
const CHECKPOINT: &str = "Metadata/MiscellaneousObjects/Checkpoint";

/// What a travel object is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Gate,
    Waypoint,
    Checkpoint { boss: bool },
    Well,
}

/// Where a gate leads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Not a gate, or nothing to lead to.
    None,
    /// Another area; `index` counts this area's earlier gates to the same area (the blob's index).
    Area { area: String, hash16: u16, index: u8 },
    /// Another place in the same instance.
    Teleport { x: i32, y: i32 },
}

/// Which rule gave a gate its target (the batch's coverage report).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rule {
    /// `entranceN`, the rule the capture shows for one room with one gate.
    Entrance,
    /// The next connection no tag names, the rule the capture shows for `G1_town`.
    NextConnection,
    /// `teleportN` to `telepadN`: from the tags only, no capture.
    Telepad,
    /// A room's gate after its tags ran out: no evidence.
    Repeat,
    /// Untagged gates paired with each other: no evidence.
    Paired,
    /// A single untagged gate: no evidence.
    FirstConnection,
    /// Not a gate, or a gate in an area with no connection and no partner.
    None,
}

/// A travel object placed in a generated area.
#[derive(Debug, Clone, PartialEq)]
pub struct WorldObject {
    pub class: Class,
    /// The type sent to the client (the placed type, or the captured one of its class).
    pub kind: String,
    /// The `.ao` the room or tile names.
    pub ao: String,
    pub x: i32,
    pub y: i32,
    pub angle: f32,
    pub target: Target,
    pub rule: Rule,
}

/// The placed object's type, by its components (`.ot`): a gate has `AreaTransition`, a waypoint
/// the `waypoint` interaction, a checkpoint a `ControlZone`, all of them something to click.
fn classify(gd: &GameData, kind: &str) -> Option<(Class, &'static str)> {
    let t = ObjectTemplate::load(gd, kind).ok()?;
    let has = |c: &str| t.components.contains_key(c);
    if !(has("Positioned") && has("Animated") && has("InteractionAction")) {
        return None;
    }
    let name = kind.rsplit('/').next().unwrap_or(kind);
    if has("AreaTransition") {
        return Some((Class::Gate, FOG_GATE));
    }
    if t.component("InteractionAction").iter().any(|(k, v)| k == "action_name" && v == "waypoint") {
        return Some((Class::Waypoint, WAYPOINT));
    }
    if has("ControlZone") && name.contains("Checkpoint") {
        return Some((Class::Checkpoint { boss: name.contains("Boss") }, CHECKPOINT));
    }
    name.contains("HealingWell").then_some((Class::Well, ""))
}

/// The type sent for a placed type: its own when it has the captured type's components.
fn sent_kind(gd: &GameData, kind: &str, captured: &str) -> String {
    let names = |k: &str| ObjectTemplate::load(gd, k).map(|t| t.components.into_keys().collect::<BTreeSet<_>>()).ok();
    match names(kind) == names(captured) {
        true => kind.to_owned(),
        false => captured.to_owned(),
    }
}

/// `prefix<digits>` at the start of a tag: the number (`teleport2disabled-Arena` gives 2).
fn tag_number(tag: &str, prefix: &str) -> Option<usize> {
    let rest = tag.strip_prefix(prefix)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn contains(room: &TaggedRoom, d: &Doodad) -> bool {
    let (x, y) = (d.x.div_euclid(TILE), d.y.div_euclid(TILE));
    x >= room.origin.0 && y >= room.origin.1 && x < room.origin.0 + room.size.0 && y < room.origin.1 + room.size.1
}

/// The room of a cell: the smallest one holding it with tags, else the smallest one holding it.
fn room_of(rooms: &[TaggedRoom], d: &Doodad) -> Option<usize> {
    let area = |r: &TaggedRoom| r.size.0 * r.size.1;
    let holding = || rooms.iter().enumerate().filter(|(_, r)| contains(r, d));
    holding().filter(|(_, r)| !r.tags.is_empty()).min_by_key(|(_, r)| area(r)).or_else(|| holding().min_by_key(|(_, r)| area(r))).map(|(i, _)| i)
}

/// What a tag leads to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lead {
    Connection(usize),
    Telepad(usize),
}

/// A room's targets, in the order its gates take them.
fn room_leads(room: &TaggedRoom, connections: usize, telepads: &BTreeMap<usize, (i32, i32)>) -> Vec<Lead> {
    let numbers = |prefix: &str| -> Vec<usize> {
        let set: BTreeSet<usize> = room.tags.iter().filter_map(|t| tag_number(t, prefix)).collect();
        set.into_iter().collect()
    };
    let mut leads: Vec<Lead> = numbers("entrance").into_iter().filter(|&n| n >= 1 && n <= connections).map(|n| Lead::Connection(n - 1)).collect();
    leads.extend(numbers("teleport").into_iter().filter(|n| telepads.contains_key(n)).map(Lead::Telepad));
    leads
}

/// Where a room tagged `telepadN` receives: a gate of that room, else its middle.
fn telepads(rooms: &[TaggedRoom], gates: &[&Doodad]) -> BTreeMap<usize, (i32, i32)> {
    let mut out = BTreeMap::new();
    for room in rooms {
        for n in room.tags.iter().filter_map(|t| tag_number(t, "telepad")) {
            let cell = gates.iter().find(|g| contains(room, g)).map_or(((room.origin.0 * 2 + room.size.0) * TILE / 2, (room.origin.1 * 2 + room.size.1) * TILE / 2), |g| (g.x, g.y));
            out.entry(n).or_insert(cell);
        }
    }
    out
}

/// The travel objects of a generated area.
pub fn travel_objects(gd: &GameData, area_id: &str, g: &Generated) -> Result<Vec<WorldObject>, GenError> {
    let data = |e: poe_data::DataError| GenError::Unsupported(format!("objects of {area_id}: {e}"));
    let table = gd.table("WorldAreas").map_err(data)?;
    let row = table.find_by_str("Id", area_id).map_err(data)?.ok_or_else(|| GenError::Unsupported(format!("no area {area_id}")))?;
    let mut connections = Vec::new();
    if let Some(list) = row.get("Connections").map_err(data)?.as_list() {
        for i in list.iter().filter_map(|v| v.as_row()) {
            let Some(r) = table.row(i) else { continue };
            connections.push((r.str("Id").map_err(data)?, r.int("HASH16").map_err(data)? as u16));
        }
    }
    let mut classes: BTreeMap<String, Option<(Class, &'static str)>> = BTreeMap::new();
    let placed: Vec<&Doodad> = g.doodads.iter().chain(&g.tile_objects).filter(|d| d.exists(Side::Server)).collect();
    let mut objects: Vec<(&Doodad, Class, &'static str)> = Vec::new();
    for d in placed {
        if let Some((class, captured)) = *classes.entry(d.kind.clone()).or_insert_with(|| classify(gd, &d.kind)) {
            objects.push((d, class, captured));
        }
    }
    let rooms = &g.map.tagged_rooms;
    let gates: Vec<&Doodad> = objects.iter().filter(|(_, c, _)| *c == Class::Gate).map(|(d, _, _)| *d).collect();
    let pads = telepads(rooms, &gates);
    let leads: Vec<Vec<Lead>> = rooms.iter().map(|r| room_leads(r, connections.len(), &pads)).collect();
    let named: BTreeSet<usize> = leads.iter().flatten().filter_map(|l| if let Lead::Connection(c) = l { Some(*c) } else { None }).collect();
    let mut free: Vec<usize> = (0..connections.len()).filter(|c| !named.contains(c)).collect();
    free.reverse();
    // First pass: tags, then free connections.
    let mut taken = vec![0usize; rooms.len()];
    let mut assigned: Vec<(Option<Lead>, Rule)> = Vec::with_capacity(gates.len());
    for gate in &gates {
        let room = room_of(rooms, gate);
        let list = room.map_or(&[][..], |r| &leads[r]);
        let n = room.map_or(0, |r| {
            taken[r] += 1;
            taken[r] - 1
        });
        assigned.push(match list.get(n) {
            Some(l @ Lead::Connection(_)) => (Some(*l), Rule::Entrance),
            Some(l @ Lead::Telepad(_)) => (Some(*l), Rule::Telepad),
            None => match free.pop() {
                Some(c) => (Some(Lead::Connection(c)), Rule::NextConnection),
                None => match list.last() {
                    Some(l) => (Some(*l), Rule::Repeat),
                    None => (None, Rule::None),
                },
            },
        });
    }
    // Second pass: gates with nothing are paired, a single one takes the first connection.
    let lonely: Vec<usize> = assigned.iter().enumerate().filter(|(_, a)| a.0.is_none()).map(|(i, _)| i).collect();
    let mut partner: BTreeMap<usize, usize> = BTreeMap::new();
    for pair in lonely.chunks(2) {
        match *pair {
            [a, b] => {
                partner.insert(a, b);
                partner.insert(b, a);
            }
            [a] if !connections.is_empty() => assigned[a] = (Some(Lead::Connection(0)), Rule::FirstConnection),
            _ => {}
        }
    }
    // The blob's index: earlier gates to the same area.
    let mut seen: BTreeMap<u16, u8> = BTreeMap::new();
    let mut gate_no = 0;
    let mut out = Vec::with_capacity(objects.len());
    for (d, class, captured) in objects {
        let (target, rule) = if class == Class::Gate {
            let i = gate_no;
            gate_no += 1;
            match (assigned[i].0, partner.get(&i)) {
                (Some(Lead::Connection(c)), _) => {
                    let (area, hash16) = connections[c].clone();
                    let index = seen.entry(hash16).or_insert(0);
                    let target = Target::Area { area, hash16, index: *index };
                    *index = index.wrapping_add(1);
                    (target, assigned[i].1)
                }
                (Some(Lead::Telepad(n)), _) => {
                    let (x, y) = pads[&n];
                    (Target::Teleport { x, y }, assigned[i].1)
                }
                (None, Some(&p)) => (Target::Teleport { x: gates[p].x, y: gates[p].y }, Rule::Paired),
                (None, None) => (Target::None, Rule::None),
            }
        } else {
            (Target::None, Rule::None)
        };
        let kind = if captured.is_empty() { d.kind.clone() } else { sent_kind(gd, &d.kind, captured) };
        out.push(WorldObject { class, kind, ao: d.path.clone(), x: d.x, y: d.y, angle: d.angle, target, rule });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_numbers() {
        assert_eq!(tag_number("teleport2disabled-Arena", "teleport"), Some(2));
        assert_eq!(tag_number("telepad13", "telepad"), Some(13));
        assert_eq!(tag_number("entrance", "entrance"), None);
        assert_eq!(tag_number("entrance_act2", "entrance"), None);
    }

    #[test]
    fn a_room_lists_entrances_then_teleports_with_a_receiver() {
        let room = TaggedRoom { origin: (0, 0), size: (1, 1), room_type: String::new(), tags: ["teleport2", "entrance2", "teleport1disabled", "entrance9", "telepad3"].map(String::from).to_vec() };
        let pads = BTreeMap::from([(1, (5, 5))]);
        assert_eq!(room_leads(&room, 3, &pads), [Lead::Connection(1), Lead::Telepad(1)]);
    }
}
