//! Gates, waypoints, checkpoints and wells in a session: spawning, touching, clicking and the
//! unlock state (docs/re/world-travel.md in the hub).

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use poe_cluster::Arrive;
use poe_data::{GameData, views::WorldArea};
use poe_proto::{
    Message as _,
    entity::{ObjectTail, travel_object_blob},
    instance::{
        AddObject, AreaLoad, ComponentDelta, InstanceChoice, InstanceList, InstanceRow, OpenPanel, SimulationHeader, TouchedObject, WarpEntry, WarpList, WarpRequest, WaypointChoice,
        WaypointMask,
    },
};
use poe_world::objects::{Class, Target, WorldObject};
use tracing::{info, warn};

use crate::{Handled, InstanceServer, LOCAL_PLAYER, exec::SessionCtx, player, spawn};

/// A travel object ready to send, before the instance gave it an entity id.
#[derive(Debug, Clone)]
pub struct PreparedObject {
    pub type_key: u32,
    pub blob: Vec<u8>,
    pub object: WorldObject,
    /// Where someone arriving at this object stands.
    pub arrival: (i32, i32),
}

/// A travel object of an instance.
#[derive(Debug, Clone)]
pub struct SpawnedObject {
    pub entity: u32,
    pub object: WorldObject,
    pub arrival: (i32, i32),
}

/// What the session asked the player to choose from in the instance list.
#[derive(Debug, Clone)]
struct PendingChoice {
    area: String,
    area_hash: u16,
    arrive: Arrive,
    tail: u8,
    instances: Vec<u32>,
}

#[derive(Debug, Default)]
struct TravelState {
    waypoints: BTreeSet<String>,
    checkpoints: BTreeSet<(i32, i32)>,
    touched: u32,
    /// The list's buttons: (index, cell).
    warps: Vec<(u8, (i32, i32))>,
    pending: Option<PendingChoice>,
}

/// A session's travel objects and unlock state.
#[derive(Debug)]
pub struct Travel {
    pub objects: Arc<Vec<SpawnedObject>>,
    state: Mutex<TravelState>,
}

impl Default for Travel {
    fn default() -> Self {
        Self { objects: Arc::new(Vec::new()), state: Mutex::default() }
    }
}

/// Which side of a gate is open, and a cell there about 30 cells out (official arrivals stood 15 to
/// 39 cells from the gate, across its facing).
pub fn gate_arrival(grid: &spawn::Grid, (x, y): (i32, i32), angle: f32) -> (i32, i32) {
    use poe_game::movement::Walkable;
    let (px, py) = (-angle.sin(), angle.cos());
    const DISTANCES: [f32; 6] = [30.0, 25.0, 35.0, 20.0, 40.0, 15.0];
    let on = |sign: f32, d: f32| (x + (sign * d * px).round() as i32, y + (sign * d * py).round() as i32);
    let open = |sign: f32| DISTANCES.iter().filter(|&&d| { let (a, b) = on(sign, d); grid.walkable(a, b) }).count();
    let sign = if open(1.0) >= open(-1.0) { 1.0 } else { -1.0 };
    DISTANCES.iter().map(|&d| on(sign, d)).find(|&(a, b)| grid.walkable(a, b)).or_else(|| grid.nearest_walkable((x, y), 60)).unwrap_or((x, y))
}

/// The blob, type key and arrival cell of every object of a layout.
pub fn prepare(objects: Vec<WorldObject>, grid: &spawn::Grid, town: bool) -> Vec<PreparedObject> {
    use poe_game::movement::Walkable;
    objects
        .into_iter()
        .map(|object| {
            let cell = (object.x, object.y);
            let tail = match (&object.class, &object.target) {
                (Class::Gate, Target::Area { hash16, index, .. }) => ObjectTail::Gate { area_hash: *hash16, index: *index, town },
                // A teleport within the instance names no area: the gate's own click decides.
                (Class::Gate, _) => ObjectTail::Gate { area_hash: 0, index: 0, town },
                (Class::Waypoint, _) => ObjectTail::Waypoint,
                (Class::Checkpoint { .. }, _) => ObjectTail::Checkpoint,
                (Class::Well, _) => ObjectTail::Well,
            };
            let arrival = match object.class {
                Class::Gate => gate_arrival(grid, cell, object.angle),
                // Official: on the waypoint's own cell.
                _ if grid.walkable(cell.0, cell.1) => cell,
                _ => grid.nearest_walkable(cell, 60).unwrap_or(cell),
            };
            PreparedObject {
                type_key: poe_data::hash::object_type_key(&object.kind),
                blob: travel_object_blob(cell, object.angle, &object.ao, tail),
                object,
                arrival,
            }
        })
        .collect()
}

/// Where a character with `arrive` stands in this layout.
pub fn arrival_cell(objects: &[PreparedObject], arrive: Arrive) -> Option<(i32, i32)> {
    match arrive {
        Arrive::Waypoint => objects.iter().find(|o| o.object.class == Class::Waypoint).map(|o| o.arrival),
        Arrive::Gate { from_hash, index } => {
            let to = |o: &&PreparedObject| matches!(o.object.target, Target::Area { hash16, .. } if hash16 == from_hash);
            let exact = objects.iter().filter(to).find(|o| matches!(o.object.target, Target::Area { index: i, .. } if i == index));
            exact.or_else(|| objects.iter().find(to)).map(|o| o.arrival)
        }
    }
}

/// `WorldAreas.Id` per waypoint mask bit: rows in table order with `HasWaypoint` and column 45 in
/// {1, 2, 31} (`0x141f68930`).
pub fn waypoint_bits(data: &GameData) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    for row in data.table("WorldAreas")?.rows() {
        if row.bool("HasWaypoint")? && matches!(row.get_index(45)?.as_int(), Some(1 | 2 | 31)) {
            out.push(row.str("Id")?);
        }
    }
    Ok(out)
}

/// The waypoint mask of a set of areas.
pub fn waypoint_mask(data: &GameData, areas: &BTreeSet<String>) -> anyhow::Result<[u8; 22]> {
    let mut mask = [0; 22];
    for (bit, id) in waypoint_bits(data)?.iter().enumerate() {
        if areas.contains(id) && bit / 8 < mask.len() {
            mask[bit / 8] |= 1 << (bit % 8);
        }
    }
    Ok(mask)
}

/// The quest flags that make unlocked areas show on the World panel (`0x140b747e0`): each area's
/// `FlagOnEntered` and `RequiredFlags`, its map pins' `QuestFlags1` and `QuestFlags2`.
pub fn quest_flags(data: &GameData, areas: &BTreeSet<String>) -> anyhow::Result<Vec<(u8, u64)>> {
    let table = data.table("WorldAreas")?;
    let mut flags = BTreeSet::new();
    let mut rows = BTreeSet::new();
    for id in areas {
        let Some(row) = table.find_by_str("Id", id)? else { continue };
        rows.insert(row.index());
        flags.extend(row.get("FlagOnEntered")?.as_row());
        if let Some(list) = row.get("RequiredFlags")?.as_list() {
            flags.extend(list.iter().filter_map(|v| v.as_row()));
        }
    }
    for pin in data.table("MapPins")?.rows() {
        if pin.row_ref("WorldArea")?.is_some_and(|r| rows.contains(&r)) {
            for column in ["QuestFlags1", "QuestFlags2"] {
                if let Some(list) = pin.get(column)?.as_list() {
                    flags.extend(list.iter().filter_map(|v| v.as_row()));
                }
            }
        }
    }
    let mut chunks: BTreeMap<u8, u64> = BTreeMap::new();
    for flag in flags {
        *chunks.entry((flag >> 6) as u8).or_default() |= 1 << (flag & 63);
    }
    Ok(chunks.into_iter().collect())
}

impl Travel {
    /// A session's state from the store. The caller's objects are the instance's.
    pub fn new(objects: Arc<Vec<SpawnedObject>>, waypoints: Vec<String>, checkpoints: Vec<(i32, i32)>) -> Self {
        Self { objects, state: Mutex::new(TravelState { waypoints: waypoints.into_iter().collect(), checkpoints: checkpoints.into_iter().collect(), ..Default::default() }) }
    }

    pub fn object(&self, entity: u32) -> Option<&SpawnedObject> {
        self.objects.iter().find(|o| o.entity == entity)
    }

    pub fn waypoints(&self) -> BTreeSet<String> {
        self.state.lock().unwrap().waypoints.clone()
    }

    /// The area map's travel list: the area's waypoint when unlocked (button 0), then the unlocked
    /// checkpoints. Remembers the cells for [`WarpRequest`].
    pub fn warp_list(&self, area: &str) -> WarpList {
        let mut state = self.state.lock().unwrap();
        let mut entries = Vec::new();
        let mut warps = Vec::new();
        if state.waypoints.contains(area)
            && let Some(o) = self.objects.iter().find(|o| o.object.class == Class::Waypoint)
        {
            entries.push(WarpEntry { boss: false, index: 0, x: o.object.x as u16, y: o.object.y as u16 });
            warps.push((0, o.arrival));
        }
        // Official lists number the checkpoints from 1, in no order the client depends on.
        let mut index = 1;
        for o in &*self.objects {
            if let Class::Checkpoint { boss } = o.object.class
                && state.checkpoints.contains(&(o.object.x, o.object.y))
            {
                entries.push(WarpEntry { boss, index, x: o.object.x as u16, y: o.object.y as u16 });
                warps.push((index, o.arrival));
                index += 1;
            }
        }
        state.warps = warps;
        WarpList { unk: 0, entries }
    }
}

/// The waypoints a choice of `unlock` or `lock` names: `all`, `acts`, `waypoints` take the areas of
/// `acts` (`[travel] acts`); `act <n>` one act; `waypoint <area>` one area.
pub fn waypoints_named(data: &GameData, acts: &[i64], what: &str, value: Option<&str>) -> anyhow::Result<Vec<String>> {
    let bits = waypoint_bits(data)?;
    let table = data.table("WorldAreas")?;
    let act_of = |id: &String| -> anyhow::Result<i64> { Ok(table.find_by_str("Id", id)?.map_or(0, |r| r.int("Act").unwrap_or(0))) };
    let mut out = Vec::new();
    match what {
        "act" => {
            let Some(n) = value.and_then(|v| v.parse::<i64>().ok()) else {
                anyhow::bail!("act needs a number");
            };
            for id in &bits {
                if act_of(id)? == n {
                    out.push(id.clone());
                }
            }
        }
        "waypoint" => {
            let Some(id) = value else {
                anyhow::bail!("waypoint needs an area");
            };
            let Some(found) = bits.iter().find(|b| b.eq_ignore_ascii_case(id)) else {
                anyhow::bail!("{id} has no waypoint");
            };
            out.push(found.clone());
        }
        _ => {
            for id in &bits {
                if acts.contains(&act_of(id)?) {
                    out.push(id.clone());
                }
            }
        }
    }
    Ok(out)
}

/// `0x2f9` on an activated checkpoint: component 4, as captured.
fn checkpoint_activated(entity: u32) -> ComponentDelta {
    ComponentDelta { header: SimulationHeader { entity_id: entity, ..Default::default() }, component: 4, delta: vec![2, 0, 1, 0, 0, 0] }
}

impl InstanceServer {
    /// Bytes for the session's client: the mask and the travel list, at entry and after an unlock.
    pub(crate) fn travel_state_bytes(&self, ctx: &SessionCtx<'_>, entity: u32) -> anyhow::Result<Vec<u8>> {
        let table = &self.protocol.instance;
        let mask = waypoint_mask(&self.data, &ctx.travel.waypoints())?;
        let mut out = table.encode(&WaypointMask { unk: 0, entity, mask })?;
        out.extend(table.encode(&ctx.travel.warp_list(&ctx.ticket.area))?);
        Ok(out)
    }

    /// Every second: what commands (of this session, other admins, the web admin) changed in the
    /// store reaches the client: the mask, the travel list and the quest flags.
    pub(crate) fn refresh_unlocks(&self, ctx: &SessionCtx<'_>, entity: u32) -> anyhow::Result<Vec<u8>> {
        let character = ctx.ticket.character.id;
        let waypoints: BTreeSet<String> = self.store.waypoints(character)?.into_iter().collect();
        let checkpoints: BTreeSet<(i32, i32)> = self.store.checkpoints(character, &ctx.ticket.area, ctx.seed)?.into_iter().collect();
        {
            let mut state = ctx.travel.state.lock().unwrap();
            if state.waypoints == waypoints && state.checkpoints == checkpoints {
                return Ok(Vec::new());
            }
            state.waypoints = waypoints.clone();
            state.checkpoints = checkpoints;
        }
        let table = &self.protocol.instance;
        let mut out = self.travel_state_bytes(ctx, 0)?;
        out.extend(table.encode(&ComponentDelta {
            header: SimulationHeader { entity_id: entity, ..Default::default() },
            component: poe_proto::entity::PLAYER_COMPONENT,
            delta: poe_proto::entity::PlayerDelta { flag_words: Some(quest_flags(&self.data, &waypoints)?), ..Default::default() }.encode(),
        })?);
        info!(waypoints = waypoints.len(), "unlock state sent");
        Ok(out)
    }

    /// `unlock`, `lock`: the store changes, the character's session follows within a second.
    pub(crate) fn unlock_command(&self, character_id: u32, unlock: bool, what: &str, value: Option<&str>) -> anyhow::Result<usize> {
        let areas = waypoints_named(&self.data, &self.config.travel.acts, what, value)?;
        match unlock {
            true => self.store.unlock_waypoints(character_id, &areas)?,
            false => self.store.lock_waypoints(character_id, Some(&areas))?,
        }
        Ok(areas.len())
    }

    /// `checkpoint unlock all`.
    pub(crate) fn unlock_instance_checkpoints(&self, ctx: &SessionCtx<'_>) -> anyhow::Result<usize> {
        let cells: Vec<(i32, i32)> = ctx.travel.objects.iter().filter(|o| matches!(o.object.class, Class::Checkpoint { .. })).map(|o| (o.object.x, o.object.y)).collect();
        self.store.unlock_checkpoints(ctx.ticket.character.id, &ctx.ticket.area, ctx.seed, &cells)?;
        Ok(cells.len())
    }

    /// Frame: the player's cell against the waypoints and checkpoints, unlocking what it touches.
    pub(crate) fn touch_objects(&self, ctx: &SessionCtx<'_>, cell: (i32, i32)) -> anyhow::Result<Vec<u8>> {
        let travel = ctx.travel;
        let range = i64::from(self.config.travel.touch_range);
        let distance = |o: &SpawnedObject| {
            let (dx, dy) = (i64::from(o.object.x - cell.0), i64::from(o.object.y - cell.1));
            dx * dx + dy * dy
        };
        let nearest = travel.objects.iter().filter(|o| matches!(o.object.class, Class::Waypoint | Class::Checkpoint { .. })).map(|o| (distance(o), o)).filter(|(d, _)| *d <= range * range).min_by_key(|(d, _)| *d).map(|(_, o)| o);
        let entity = nearest.map_or(0, |o| o.entity);
        {
            let mut state = travel.state.lock().unwrap();
            if state.touched == entity {
                return Ok(Vec::new());
            }
            state.touched = entity;
        }
        let table = &self.protocol.instance;
        let mut out = table.encode(&TouchedObject { local: LOCAL_PLAYER, entity })?;
        let Some(o) = nearest else {
            return Ok(out);
        };
        let character = ctx.ticket.character.id;
        let area = &ctx.ticket.area;
        match o.object.class {
            Class::Waypoint => {
                let new = travel.state.lock().unwrap().waypoints.insert(area.clone());
                if new {
                    self.store.unlock_waypoints(character, std::slice::from_ref(area))?;
                    out.extend(self.travel_state_bytes(ctx, 0)?);
                    info!(%area, "waypoint unlocked");
                }
            }
            Class::Checkpoint { .. } => {
                let at = (o.object.x, o.object.y);
                let new = travel.state.lock().unwrap().checkpoints.insert(at);
                if new {
                    self.store.unlock_checkpoints(character, area, ctx.seed, &[at])?;
                    out.extend(table.encode(&checkpoint_activated(o.entity))?);
                    out.extend(table.encode(&travel.warp_list(area))?);
                    info!(%area, cell = ?at, "checkpoint unlocked");
                }
                self.store.set_respawn(character, &poe_store::Respawn { area: area.clone(), seed: ctx.seed, x: o.arrival.0, y: o.arrival.1 })?;
            }
            _ => {}
        }
        Ok(out)
    }

    /// A click on an object, once the player is in reach. `true`: the session ends (hand-off).
    pub(crate) fn interact(&self, ctx: &SessionCtx<'_>, player: &player::Player, entity: u32, flags: u16) -> anyhow::Result<(Vec<u8>, bool)> {
        let table = &self.protocol.instance;
        let Some(o) = ctx.travel.object(entity) else {
            return Ok((Vec::new(), false));
        };
        info!(?o.object.class, ?o.object.target, cell = ?(o.object.x, o.object.y), flags, "object clicked");
        match (&o.object.class, &o.object.target) {
            (Class::Gate, Target::Area { area, hash16, .. }) => {
                let here = WorldArea::find(&self.data, &ctx.ticket.area)?.map_or(0, |a| a.hash16);
                let index = if let Target::Area { index, .. } = o.object.target { index } else { 0 };
                let arrive = Arrive::Gate { from_hash: here, index };
                // The capture's ctrl-click: bit 2 of the request's flags.
                if flags & 0x0004 != 0 {
                    return Ok((self.instance_choice(ctx, area, *hash16, arrive, 0)?, false));
                }
                Ok((self.go_to_area(ctx, area, *hash16, arrive, 0, false)?, true))
            }
            (Class::Gate, Target::Teleport { x, y }) => {
                // The arrival of the gate standing there, else the target cell.
                let cell = ctx.travel.objects.iter().find(|p| p.object.class == Class::Gate && (p.object.x, p.object.y) == (*x, *y)).map_or((*x, *y), |p| p.arrival);
                player.teleport(cell);
                Ok((Vec::new(), false))
            }
            (Class::Gate, _) => Ok((Vec::new(), false)),
            (Class::Waypoint, _) => {
                let mut out = self.travel_state_bytes(ctx, entity)?;
                out.extend(table.encode(&OpenPanel { local: LOCAL_PLAYER, sub: OpenPanel::WORLD, a: entity, ..Default::default() })?);
                Ok((out, false))
            }
            (Class::Checkpoint { .. }, _) => {
                let mut out = table.encode(&ctx.travel.warp_list(&ctx.ticket.area))?;
                out.extend(table.encode(&OpenPanel { local: LOCAL_PLAYER, sub: OpenPanel::AREA_MAP, ..Default::default() })?);
                Ok((out, false))
            }
            // Refills life, mana and flasks once they exist (P13, P14).
            (Class::Well, _) => Ok((Vec::new(), false)),
        }
    }

    /// Starts the load and hands the client to the area's open instance, or a new one.
    fn go_to_area(&self, ctx: &SessionCtx<'_>, area: &str, area_hash: u16, arrive: Arrive, tail: u8, new: bool) -> anyhow::Result<Vec<u8>> {
        let id = match self.instances.find(area).filter(|_| !new) {
            Some(id) => id,
            None => self.create_area_instance(area)?,
        };
        self.leave_to(ctx, id, area_hash, arrive, tail)
    }

    fn create_area_instance(&self, area: &str) -> anyhow::Result<u32> {
        let found = WorldArea::find(&self.data, area)?;
        let seed = found.and_then(|a| a.fixed_seed());
        Ok(self.instances.create(poe_cluster::InstanceSpec { area: area.to_owned(), seed, topology: None, orientation: None }, false))
    }

    fn leave_to(&self, ctx: &SessionCtx<'_>, id: u32, area_hash: u16, arrive: Arrive, tail: u8) -> anyhow::Result<Vec<u8>> {
        let table = &self.protocol.instance;
        let mut out = table.encode(&TouchedObject { local: LOCAL_PLAYER, entity: 0 })?;
        out.extend(table.encode(&AreaLoad { unk: 0, area_hash, tail })?);
        let mut character = ctx.ticket.character.clone();
        character.location = None;
        character.arrive = Some(arrive);
        self.hand_off_at(ctx.ticket.account_id, character, id, None)?;
        Ok(out)
    }

    /// The instance list a ctrl-click asks for.
    fn instance_choice(&self, ctx: &SessionCtx<'_>, area: &str, area_hash: u16, arrive: Arrive, tail: u8) -> anyhow::Result<Vec<u8>> {
        let open: Vec<_> = self.instances.list().into_iter().filter(|i| i.spec.area == area).collect();
        let level = WorldArea::find(&self.data, area)?.map_or(1, |a| a.level as u8);
        let rows = open.iter().map(|i| InstanceRow { id: u64::from(i.id), unk: 0, level, names: i.players.clone() }).collect();
        info!(%area, open = open.len(), "instance list sent");
        ctx.travel.state.lock().unwrap().pending = Some(PendingChoice { area: area.to_owned(), area_hash, arrive, tail, instances: open.iter().map(|i| i.id).collect() });
        Ok(self.protocol.instance.encode(&InstanceList { local: LOCAL_PLAYER, area_hash, level, rows })?)
    }

    /// The client's answers to [`Self::interact`]. `None`: not such a message.
    pub(crate) async fn travel_message(&self, conn: &mut poe_net::Connection, ctx: &SessionCtx<'_>, player: &player::Player, name: &str, r: &mut poe_net::Reader<'_>) -> anyhow::Result<Option<Handled>> {
        match name {
            WaypointChoice::NAME => {
                let m = WaypointChoice::decode(r)?;
                let Some(area) = area_by_hash(&self.data, m.area_hash)? else {
                    warn!(hash = m.area_hash, "waypoint choice: no such area");
                    return Ok(Some(Handled::Continue));
                };
                if !ctx.travel.waypoints().contains(&area) {
                    warn!(%area, "waypoint choice: not unlocked");
                    return Ok(Some(Handled::Continue));
                }
                if m.flags & 0x80 != 0 {
                    conn.send(&self.instance_choice(ctx, &area, m.area_hash, Arrive::Waypoint, 2)?).await?;
                    return Ok(Some(Handled::Continue));
                }
                conn.send(&self.go_to_area(ctx, &area, m.area_hash, Arrive::Waypoint, 2, false)?).await?;
                Ok(Some(Handled::Exit))
            }
            InstanceChoice::NAME => {
                let m = InstanceChoice::decode(r)?;
                info!(choice = m.choice, "instance choice");
                let Some(p) = ctx.travel.state.lock().unwrap().pending.take() else {
                    return Ok(Some(Handled::Continue));
                };
                let id = match p.instances.get(m.choice as usize) {
                    Some(id) => *id,
                    None => self.create_area_instance(&p.area)?,
                };
                conn.send(&self.leave_to(ctx, id, p.area_hash, p.arrive, p.tail)?).await?;
                Ok(Some(Handled::Exit))
            }
            WarpRequest::NAME => {
                let m = WarpRequest::decode(r)?;
                let cell = ctx.travel.state.lock().unwrap().warps.iter().find(|(i, _)| *i == m.index).map(|(_, c)| *c);
                match cell {
                    Some(cell) => {
                        info!(index = m.index, ?cell, "warp request");
                        player.teleport(cell)
                    }
                    None => warn!(index = m.index, "warp request: no such button"),
                }
                Ok(Some(Handled::Continue))
            }
            _ => Ok(None),
        }
    }
}

fn area_by_hash(data: &GameData, hash: u16) -> anyhow::Result<Option<String>> {
    for row in data.table("WorldAreas")?.rows() {
        if row.int("HASH16")? as u16 == hash {
            return Ok(Some(row.str("Id")?));
        }
    }
    Ok(None)
}

/// The `AddObject`s of an instance's objects (header as the players').
pub fn add_objects(entity: u32, key: u32, blob: Vec<u8>, after_all: u32) -> AddObject {
    AddObject { header: SimulationHeader { entity_id: entity, unk_14: after_all, ..Default::default() }, type_key: key, blob }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(class: Class, target: Target, cell: (i32, i32)) -> PreparedObject {
        PreparedObject {
            type_key: 0,
            blob: Vec::new(),
            object: WorldObject { class, kind: String::new(), ao: String::new(), x: cell.0, y: cell.1, angle: 0.0, target, rule: poe_world::objects::Rule::None },
            arrival: cell,
        }
    }

    fn gate(hash16: u16, index: u8, cell: (i32, i32)) -> PreparedObject {
        object(Class::Gate, Target::Area { area: String::new(), hash16, index }, cell)
    }

    #[test]
    fn a_gate_arrival_takes_the_gate_back_with_the_same_index_or_the_first() {
        let list = [gate(7, 0, (1, 1)), gate(7, 1, (2, 2)), object(Class::Waypoint, Target::None, (3, 3))];
        assert_eq!(arrival_cell(&list, Arrive::Gate { from_hash: 7, index: 1 }), Some((2, 2)));
        assert_eq!(arrival_cell(&list, Arrive::Gate { from_hash: 7, index: 5 }), Some((1, 1)));
        assert_eq!(arrival_cell(&list, Arrive::Gate { from_hash: 8, index: 0 }), None);
        assert_eq!(arrival_cell(&list, Arrive::Waypoint), Some((3, 3)));
    }

    #[test]
    fn the_warp_list_starts_with_an_unlocked_waypoint_and_keeps_only_unlocked_checkpoints() {
        let spawned = |entity, class, cell: (i32, i32)| SpawnedObject {
            entity,
            object: WorldObject { class, kind: String::new(), ao: String::new(), x: cell.0, y: cell.1, angle: 0.0, target: Target::None, rule: poe_world::objects::Rule::None },
            arrival: (cell.0 + 1, cell.1),
        };
        let objects = vec![spawned(1, Class::Checkpoint { boss: true }, (10, 10)), spawned(2, Class::Checkpoint { boss: false }, (20, 20)), spawned(3, Class::Waypoint, (30, 30))];
        let travel = Travel::new(Arc::new(objects), vec!["A".into()], vec![(10, 10)]);
        let list = travel.warp_list("A");
        assert_eq!(list.entries, [WarpEntry { boss: false, index: 0, x: 30, y: 30 }, WarpEntry { boss: true, index: 1, x: 10, y: 10 }]);
        assert_eq!(travel.state.lock().unwrap().warps, [(0, (31, 30)), (1, (11, 10))]);
        // Without the waypoint the checkpoint keeps index 1.
        assert_eq!(Travel::new(travel.objects.clone(), Vec::new(), vec![(10, 10)]).warp_list("A").entries[0].index, 1);
    }
}
