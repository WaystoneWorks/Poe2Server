//! The instance world: entities with unique ids, one frame clock, and their components.
//!
//! One [`World`] belongs to one instance. Sessions feed it client input
//! ([`World::input`]), the instance's tick loop calls [`World::tick`] once per frame, and the
//! returned [`WorldEvent`]s are what the clients must be told, in order. The world never
//! reads a clock: the server owns timing and calls `tick` every `tick_ms`.
//!
//! Entities are plain structs with their components as fields. Today the only component is the
//! entity's [`Mover`], made by the world's [`MovementModel`] (the movement seam,
//! [`crate::movement`]); more (stats, skills, AI) get their own fields and event variants. Every
//! player entity of the instance is spawned into the same world and gets its own mover.

use std::{collections::BTreeMap, sync::Arc};

use crate::{
    grid::GridSource,
    movement::{Cell, Drain, MoveEvent, MoveInput, Mover, MovementConfig, MovementModel, StepCtx},
};

/// An entity id, unique within a world (the protocol's `u32` entity id).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntityId(pub u32);

#[derive(Debug, Clone)]
pub struct WorldConfig {
    /// Milliseconds per frame (`tick_ms`); the simulation advances `tick_ms / 1000` seconds per
    /// tick.
    pub tick_ms: u32,
    /// The first entity id handed out.
    pub first_entity_id: u32,
    /// The first `StartAction` id (one counter for the whole world, shared by all entities).
    pub first_action_id: u16,
    pub movement: MovementConfig,
}

impl WorldConfig {
    /// Official defaults: 33 ms frames, entity ids from 1, action ids from 32768.
    pub fn new(movement: MovementConfig) -> Self {
        Self {
            tick_ms: 33,
            first_entity_id: 1,
            first_action_id: 32768,
            movement,
        }
    }
}

/// An entity and its components.
#[derive(Debug)]
pub struct Entity {
    pub id: EntityId,
    /// Next free buff index of this entity (shared by all components that add buffs).
    pub next_buff: u16,
    pub movement: Box<dyn Mover>,
    /// This entity's own movement configuration (its class's game data); `None`: the world's.
    pub config: Option<MovementConfig>,
}

/// Something that happened to an entity this frame and that clients must be told.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Event {
    Movement(MoveEvent),
}

/// An [`Event`] with the frame (batch) it belongs to and the entity it concerns.
#[derive(Debug, Clone, PartialEq)]
pub struct WorldEvent {
    pub frame: u64,
    pub entity: EntityId,
    /// Its place in the client's frame.
    pub key: Drain,
    pub event: Event,
}

impl WorldEvent {
    pub fn movement(&self) -> Option<&MoveEvent> {
        match &self.event {
            Event::Movement(m) => Some(m),
        }
    }
}

pub struct World {
    config: WorldConfig,
    model: Arc<dyn MovementModel>,
    grid: Arc<dyn GridSource>,
    frame: u64,
    next_entity: u32,
    next_action: u16,
    entities: BTreeMap<EntityId, Entity>,
    paused: bool,
    objects: crate::movement::ObjectReaches,
}

impl World {
    pub fn new(
        config: WorldConfig,
        model: Arc<dyn MovementModel>,
        grid: Arc<dyn GridSource>,
    ) -> Self {
        Self {
            model,
            next_entity: config.first_entity_id,
            next_action: config.first_action_id,
            config,
            grid,
            frame: 0,
            entities: BTreeMap::new(),
            paused: false,
            objects: Default::default(),
        }
    }

    /// The clickable objects of this world (`ActorSpawn`: sent once per instance).
    pub fn set_objects(&mut self, objects: crate::movement::ObjectReaches) {
        self.objects = objects;
    }

    /// Frames ticked so far (the number of the last batch).
    pub fn frame(&self) -> u64 {
        self.frame
    }

    pub fn tick_ms(&self) -> u32 {
        self.config.tick_ms
    }

    /// Changes the frame length from the next frame on (`time scale`; the frame rate stays with
    /// the server's clock).
    pub fn set_tick_ms(&mut self, tick_ms: u32) {
        self.config.tick_ms = tick_ms;
    }

    pub fn config(&self) -> &WorldConfig {
        &self.config
    }

    pub fn model(&self) -> &Arc<dyn MovementModel> {
        &self.model
    }

    pub fn grid(&self) -> &Arc<dyn GridSource> {
        &self.grid
    }

    /// Swaps the terrain (the area's grid arrived after the world was created).
    pub fn set_grid(&mut self, grid: Arc<dyn GridSource>) {
        self.grid = grid;
    }

    /// While paused, frames still count and movers get [`Mover::tick_paused`] (solo escape
    /// menu).
    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    pub fn paused(&self) -> bool {
        self.paused
    }

    /// Adds a player entity standing on `cell` with the world's movement configuration.
    pub fn spawn_player(&mut self, cell: Cell) -> EntityId {
        self.spawn_player_with_config(cell, None)
    }

    /// Like [`spawn_player`](Self::spawn_player) with the entity's own movement configuration
    /// (`None`: the world's). Players of different classes share one world, so the
    /// class-dependent values live on the entity.
    /// An entity id for something the world does not simulate (a travel object).
    pub fn reserve_entity(&mut self) -> EntityId {
        let id = EntityId(self.next_entity);
        self.next_entity += 1;
        id
    }

    pub fn spawn_player_with_config(
        &mut self,
        cell: Cell,
        config: Option<MovementConfig>,
    ) -> EntityId {
        let id = EntityId(self.next_entity);
        self.next_entity += 1;
        let movement = self
            .model
            .spawn(cell, config.as_ref().unwrap_or(&self.config.movement));
        self.entities.insert(
            id,
            Entity {
                id,
                next_buff: 0,
                movement,
                config,
            },
        );
        id
    }

    pub fn despawn(&mut self, id: EntityId) -> Option<Entity> {
        self.entities.remove(&id)
    }

    pub fn entity(&self, id: EntityId) -> Option<&Entity> {
        self.entities.get(&id)
    }

    pub fn entity_mut(&mut self, id: EntityId) -> Option<&mut Entity> {
        self.entities.get_mut(&id)
    }

    pub fn entities(&self) -> impl Iterator<Item = &Entity> {
        self.entities.values()
    }

    /// Queues client input for an entity; it is answered in the next tick. Returns whether the
    /// entity exists.
    pub fn input(&mut self, id: EntityId, input: MoveInput) -> bool {
        match self.entities.get_mut(&id) {
            Some(e) => {
                e.movement.input(input);
                true
            }
            None => false,
        }
    }

    /// The entity's speed stats changed (gear, buffs, a dev command): the next tick moves at the
    /// new speed and sends the results to the clients ([`Mover::set_speed_stats`]).
    pub fn set_speed_stats(&mut self, id: EntityId, stats: crate::movement::SpeedStats) -> bool {
        match self.entities.get_mut(&id) {
            Some(e) => {
                e.movement.set_speed_stats(stats);
                true
            }
            None => false,
        }
    }

    /// Places an entity (the next tick reports it with [`MoveEvent::Teleport`]).
    pub fn teleport(&mut self, id: EntityId, cell: Cell) -> bool {
        match self.entities.get_mut(&id) {
            Some(e) => {
                e.movement.teleport(cell);
                true
            }
            None => false,
        }
    }

    /// Advances one frame and returns what happened, in the order the client drains it: order key
    /// 0 first, then by entity, component and sequence, then the end of the frame. The client's
    /// drain only looks at the head of its queue, so a message out of order would hold back
    /// all that follow it.
    pub fn tick(&mut self) -> Vec<WorldEvent> {
        self.frame += 1;
        let paused = self.paused;
        let dt = self.config.tick_ms as f32 / 1000.0;
        let mut out = Vec::new();
        for e in self.entities.values_mut() {
            let mut ctx = StepCtx {
                frame: self.frame,
                dt,
                terrain: &*self.grid,
                config: e.config.as_ref().unwrap_or(&self.config.movement),
                next_action: &mut self.next_action,
                next_buff: &mut e.next_buff,
                objects: &self.objects,
            };
            let events = if paused {
                e.movement.tick_paused(&mut ctx)
            } else {
                e.movement.tick(&mut ctx)
            };
            for keyed in events {
                out.push(WorldEvent {
                    frame: self.frame,
                    entity: e.id,
                    key: keyed.key,
                    event: Event::Movement(keyed.event),
                });
            }
        }
        out.sort_by_key(|e| match e.key {
            Drain::Start => (0, 0, 0, 0),
            Drain::Component { comp, seq } => (1, e.entity.0, comp, seq),
            Drain::End => (2, 0, 0, 0),
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        grid::VecGrid,
        movement::{Keyed, Open, SkillIds},
        standstill::StandStillModel,
    };

    /// Records what the world hands its movers: one event per input, echoing the frame's action
    /// and buff counters, so the tests see the world's bookkeeping.
    #[derive(Debug)]
    struct Probe {
        cell: Cell,
        inputs: Vec<MoveInput>,
    }

    impl Mover for Probe {
        fn input(&mut self, input: MoveInput) {
            self.inputs.push(input);
        }
        fn teleport(&mut self, cell: Cell) {
            self.cell = cell;
        }
        fn tick(&mut self, ctx: &mut StepCtx) -> Vec<Keyed> {
            let blocked = !ctx.terrain.walkable(self.cell.0, self.cell.1);
            self.inputs
                .drain(..)
                .map(|_| {
                    let action = *ctx.next_action;
                    *ctx.next_action += 1;
                    let index = *ctx.next_buff;
                    *ctx.next_buff += 1;
                    MoveEvent::BuffAdded {
                        index,
                        row: action,
                        fade: blocked.then_some(ctx.dt),
                    }
                    .into()
                })
                .collect()
        }
        fn cell(&self) -> Cell {
            self.cell
        }
    }

    struct ProbeModel;

    impl MovementModel for ProbeModel {
        fn name(&self) -> &'static str {
            "probe"
        }
        fn spawn(&self, cell: Cell, _: &MovementConfig) -> Box<dyn Mover> {
            Box::new(Probe {
                cell,
                inputs: Vec::new(),
            })
        }
    }

    fn config() -> WorldConfig {
        WorldConfig::new(MovementConfig::new(
            SkillIds {
                move_skill: 0x2000_2909,
                roll_skill: 0x8000_0040,
                roll_skill_other_set: u32::MAX,
            },
            [1, 2, 3],
        ))
    }

    fn world() -> World {
        World::new(config(), Arc::new(ProbeModel), Arc::new(Open))
    }

    const WALK: MoveInput = MoveInput::Direction { x: 180, y: 0 };

    #[test]
    fn entity_ids_are_unique_per_world_and_start_where_configured() {
        let mut w = world();
        let a = w.spawn_player((0, 0));
        let b = w.spawn_player((5, 5));
        assert_eq!((a, b), (EntityId(1), EntityId(2)));
        w.despawn(a);
        // Ids are not reused.
        assert_eq!(w.spawn_player((1, 1)), EntityId(3));
        assert_eq!(w.entities().count(), 2);
        assert!(!w.input(a, MoveInput::SkillInputEnd));
        let mut cfg = config();
        cfg.first_entity_id = 100;
        let mut other = World::new(cfg, Arc::new(ProbeModel), Arc::new(Open));
        assert_eq!(other.spawn_player((0, 0)), EntityId(100));
    }

    #[test]
    fn input_is_answered_by_the_next_tick_in_entity_order() {
        let mut w = world();
        let a = w.spawn_player((10, 10));
        let b = w.spawn_player((100, 100));
        assert_eq!(w.frame(), 0);
        assert!(w.tick().is_empty());
        assert_eq!(w.frame(), 1);
        w.input(b, WALK);
        w.input(a, WALK);
        let ev = w.tick();
        let who: Vec<_> = ev.iter().map(|e| (e.frame, e.entity)).collect();
        assert_eq!(who, [(2, a), (2, b)]);
    }

    #[test]
    fn action_ids_are_one_counter_for_the_world_and_buffs_count_per_entity() {
        let mut w = world();
        let a = w.spawn_player((0, 0));
        let b = w.spawn_player((50, 50));
        for p in [a, b, a] {
            w.input(p, WALK);
        }
        let got: Vec<_> = w
            .tick()
            .iter()
            .map(|e| match e.movement() {
                Some(MoveEvent::BuffAdded { index, row, .. }) => (e.entity, *index, *row),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(got, [(a, 0, 32768), (a, 1, 32769), (b, 0, 32770)]);
    }

    #[test]
    fn pause_hands_movers_the_paused_tick() {
        let mut w = world();
        let p = w.spawn_player((10, 10));
        w.input(p, WALK);
        w.set_paused(true);
        assert!(w.tick().is_empty());
        assert_eq!(w.frame(), 1);
        w.set_paused(false);
        assert_eq!(w.tick().len(), 1, "the input waited");
    }

    #[test]
    fn movers_see_the_swapped_grid_and_tick_ms() {
        let mut w = world();
        w.config.tick_ms = 50;
        let p = w.spawn_player((45, 10));
        let mut grid = VecGrid::open(100, 100);
        grid.set(45, 10, false);
        w.set_grid(Arc::new(grid));
        w.input(p, WALK);
        assert!(matches!(
            w.tick()[0].movement(),
            Some(MoveEvent::BuffAdded { fade: Some(dt), .. }) if (*dt - 0.05).abs() < 1e-6
        ));
    }

    #[test]
    fn the_stub_model_stands_and_teleports() {
        let mut w = World::new(config(), Arc::new(StandStillModel), Arc::new(Open));
        let p = w.spawn_player((10, 10));
        w.input(p, WALK);
        assert!(w.tick().is_empty());
        assert!(w.teleport(p, (40, 40)));
        let ev = w.tick();
        assert_eq!(
            ev[0].movement(),
            Some(&MoveEvent::Teleport { cell: (40, 40) })
        );
        assert_eq!(w.entity(p).unwrap().movement.cell(), (40, 40));
    }
}
