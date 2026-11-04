//! The server's movement component: the official server's message rules on top of the exact port
//! of the client's movement ([`ClientSim`]).
//!
//! The client runs in lockstep: it moves only by what the server tells it. So the server runs the
//! client's code on the very messages it sends, at the place in the frame the client runs them
//! (the header's order key, [`Drain`]). The model's grid is then the client's grid, and a `0x64`
//! never snaps. A frame:
//!
//! 1. inputs, answered at order key 0: applied to the model at once (before its Positioned tick);
//! 2. the Positioned tick, a wall's sprint-off (`(player, 0, 4)`), the Pathfinding tick;
//! 3. the timeline (buffs `(player, 3, 0)`, roll and sprint `(player, 5, *)`), the Animated tick;
//! 4. the `0x64` with the grid after the Positioned tick (`(player, 9, 0)`), the server's own
//!    starts `(player, 9, 1)`, sprint end `(player, 9, 7)`, the Actor tick.
//!
//! The message rules are measured on official captures (the comments name the capture); the frame
//! counts below are the official server's own timeline and are not client code. The roll's sprint
//! start and end come from the model's clip, so they follow the animation's clock exactly.
//! Spec: hub `docs/protocol/movement.md`, `docs/re/wasd-movement.md` ("Frame drain").

use crate::clientsim::ClientSim;
use crate::movement::{
    SpeedStats,
    ACTION_INPUT_FLAG, ActionKind, Cell, Drain, Keyed, MoveEvent, MoveInput, MoveStat, Mover,
    MovementConfig, MovementModel, SkillIds, StepCtx, UseSkill,
};

/// Frames after the roll start at which a queued attack cuts the roll short and starts
/// (2026-10-20 capture: 24 in all 3 cases; the client's roll action ends then too).
const ATTACK_CANCEL_FRAMES: u64 = 24;
/// Frames after the roll start at which a tap roll (key up before the sprint starts) clears
/// `is_dodge_rolling`; the action itself goes on.
const TAP_END_FRAMES: u64 = 16;
/// Value of the three stun threshold stats while rolling.
const ROLL_STUN_THRESHOLD: i32 = 100;
/// While the click is held, the server re-issues the Move `StartAction` towards the latest target
/// when this many frames passed since the hold's last Move start with no new press or repeat in
/// between (all 52 restarts of the 5 held clicks in the 2026-10-20 and 2026-11-02 captures).
const CLICK_RESTART_FRAMES: u64 = 2;
/// Fade time of the first sprint buff when the roll ends.
const SPRINT_BUFF_FADE_SECS: f32 = 0.1;
/// Frames after the roll start at which `must_sprint` goes off.
const MUST_SPRINT_END_FRAMES: u64 = 35;
/// Direction vectors in sprint replies are the input scaled to this length and truncated toward
/// zero (measured on 294 of 309 replies, the rest lag one input).
const SPRINT_DIR_LENGTH: i32 = 550;
/// `sprint_blend_time_override_ms`.
const SPRINT_BLEND_OVERRIDE_MS: i32 = 1;
/// Frames from a wall's sprint-off batch to its `all_hits_stun_you` off (official 7 to 8).
const WALL_STUN_OFF_FRAMES: u64 = 7;
/// A zero WASD vector this many frames after the sprint-off batch, after a sprint that followed
/// the cursor (Space held, no key), means the keys were already up when the unscaled `MoveAck`
/// reached the client (it sends the zero vector 1 to 5 frames after the Space release). In
/// click-to-move mode this is how long cursor vectors still in flight are ignored.
const RELEASE_WINDOW_FRAMES: u64 = 6;
/// A key-shaped `0x63` vector counts as the cursor direction when the roll key's cursor offset
/// points within this many degrees of it (they differ by up to about 6 degrees).
const CURSOR_MATCH_DEGREES: f64 = 10.0;
/// Frames between a later input-mode toggle and the stance change (2026-10-20 capture: 0).
const STANCE_TOGGLE_FRAMES: u64 = 0;
/// `0x2a6` value sent with a change to combat stance (all eight sessions).
const STANCE_LERP_COMBAT: f32 = 0.05;
/// `0x2a6` value sent with a change out of combat stance (all eight sessions).
const STANCE_LERP_PEACEFUL: f32 = 0.2;
/// Places in the client's frame the official server uses (hub `docs/re/wasd-movement.md`).
const AT_WALL: Drain = Drain::at(0, 4);
const AT_BUFFS: Drain = Drain::at(3, 0);
const AT_ANIMATED: Drain = Drain::at(5, 2);
const AT_ROLL_STUN: Drain = Drain::at(5, 6);
const AT_ROLL_END_MOVE: Drain = Drain::at(5, 8);
const AT_RESTART: Drain = Drain::at(9, 1);
const AT_SPRINT_END_MOVE: Drain = Drain::at(9, 4);
const AT_SPRINT_END: Drain = Drain::at(9, 7);
const AT_SPRINT_END_LATE: Drain = Drain::at(9, 10);
const AT_LAST_BEFORE_ACTOR: Drain = Drain::at(9, u16::MAX);
const AT_LAST_BEFORE_ANIMATED: Drain = Drain::at(5, u16::MAX);

/// What a frame sends, each event with its key, and which of them the model has run.
struct Out {
    items: Vec<Keyed>,
    applied: Vec<bool>,
    /// The key of what is pushed now.
    key: Drain,
}

impl Out {
    fn new() -> Self {
        Self {
            items: Vec::new(),
            applied: Vec::new(),
            key: Drain::Start,
        }
    }

    /// Adds an event at the current key (events with a place of their own go there).
    fn push(&mut self, event: MoveEvent) {
        let key = match Drain::default_for(&event) {
            Drain::Start => self.key,
            own => own,
        };
        self.push_at(key, event);
    }

    fn push_at(&mut self, key: Drain, event: MoveEvent) {
        self.items.push(Keyed { key, event });
        self.applied.push(false);
    }

    /// In the client's order: the frame's queue is sorted (stable, so same keys keep their order).
    fn finish(self) -> Vec<Keyed> {
        let mut items = self.items;
        items.sort_by_key(|k| k.key);
        items
    }
}

/// The exact port as a [`MovementModel`].
#[derive(Debug, Clone, Copy, Default)]
pub struct ClientModel;

impl MovementModel for ClientModel {
    fn name(&self) -> &'static str {
        "clientsim"
    }

    fn spawn(&self, cell: Cell, config: &MovementConfig) -> Box<dyn Mover> {
        Box::new(Locomotion::new(cell, config))
    }
}

/// How a walk is driven.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Walk {
    /// WASD: keep going along this vector.
    Direction(Cell),
    /// Click-to-move: walk to a cell along an A* path ([`Locomotion::path`]).
    Target { goal: Cell },
}

/// A click that is being held (between the press and `SkillReleased` of the Move skill).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MoveHold {
    /// Absolute cell of the latest press or repeat.
    pub goal: Cell,
    /// Flags of the latest request (press `0x400`, repeats `0`): restarts carry them.
    pub flags: u16,
    /// Press counter; stale restarts of an earlier hold are ignored.
    seq: u64,
    /// Frame of the latest press or repeat. Repeats come every 2 frames; a hold that is
    /// quiet for longer than [`MoveHold::STALE_FRAMES`] is over (5 of 12 holds in the
    /// 2026-10-20 capture ended without a `SkillReleased`).
    last_input: u64,
    /// Frame of the hold's latest Move start (press, repeat or restart).
    last_start: u64,
    /// The latest three goals with the frame each was handled, newest first: the
    /// `StartAction` at `must_sprint` off takes an older one ([`MoveHold::settled_goal`]).
    history: [(u64, Cell); 3],
}

impl MoveHold {
    pub const STALE_FRAMES: u64 = 6;

    /// The newest goal handled at least 3 frames before `frame` (`older` = 0: 2026-11-03 f3468,
    /// back 4 with one at back 2), or the one before the newest at least 2 back (`older` = 1:
    /// 2026-10-20 f3643, 2026-11-03 f3904). Official, a sprint that goes on with a held click
    /// (2026-10-20 f3779, f4141, f4252, f4522; 2026-10-20_21-51-49 f8873, f9154; 2026-11-02
    /// f7746): a repeat handled 0 or 1 frame before `must_sprint` off is not in its
    /// `StartAction` yet (`older` 0, 7 of 7). The one sprint that ended there by an earlier
    /// release (2026-10-20 f3643) took the goal before that (`older` 1).
    fn settled_goal(&self, frame: u64, older: usize) -> Cell {
        let i = self
            .history
            .iter()
            .position(|(f, _)| f + 3 - older as u64 <= frame)
            .unwrap_or(self.history.len() - 1);
        self.history[(i + older).min(self.history.len() - 1)].1
    }
}

/// A running sprint (the buffs are added at roll start + 15 frames).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SprintRun {
    /// Frame of the roll that started it (`t0`).
    pub start_frame: u64,
    /// Buff index `n`; the sprint uses `n`, `n + 1`, `n + 2`.
    pub buff_base: u16,
    /// A non-zero WASD vector arrived after the sprint started: one `MoveAck` per frame.
    pub streaming: bool,
}

/// A dodge roll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rolling {
    pub start_frame: u64,
    /// The sprint key is down.
    pub held: bool,
    /// Cursor offset the roll runs towards when no WASD key is held (relative to the cell it
    /// started from); steered by `UpdateSkillTarget`.
    pub offset: Cell,
    /// A WASD key was held when the roll was asked for: the roll runs along the WASD vector
    /// (the client ignores the cursor then) and follows its changes (2026-10-20 capture:
    /// 14 of 14 rolls with WASD held, direction changes 2 frames after the `0x63`).
    pub wasd_driven: bool,
    pub action: u16,
    /// The roll turned into a sprint (the key was held at `start_frame + 15`).
    pub sprint: Option<SprintRun>,
    /// A sprint was started during this roll (even if it has ended again).
    pub sprinted: bool,
    /// The roll key went up since the press: the roll stays on the tap curve.
    pub released: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveState {
    Idle,
    Walking(Walk),
    Rolling(Rolling),
    /// The roll is over and the sprint goes on, until the key is released or an attack.
    Sprinting(SprintRun),
}

/// The frame a sprint ended by the key release (or the roll's end after an earlier release) and
/// the direction it ran in.
#[derive(Debug, Clone, Copy, PartialEq)]
struct SprintEnd {
    frame: u64,
    dir: Cell,
    /// The sprint's last vector was the cursor direction, not a key (see
    /// [`Locomotion::is_cursor_vector`]): the keys are up, so the client coasts.
    cursor: bool,
    /// The sprint coasted at once, without waiting for the client's zero vector: click-to-move
    /// mode, where there are no keys at all.
    coasting: bool,
    /// A held click's Move ran the sprint: its end answered with `(0, 0)` and the cursor
    /// vectors still in flight are not answered.
    click: bool,
}

/// Scheduled timeline events.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Due {
    /// A tap roll clears `is_dodge_rolling` (skipped when the roll became a sprint).
    RollEndTap {
        roll: u64,
    },
    /// `must_sprint` off; the buff index is in [`Locomotion::must_sprint_off`].
    MustSprintOff,
    /// The reply of the frame after a sprint ended: the unscaled direction.
    UnscaledAck,
    /// A `(0, 0)` `0x64` ends the direction of a sprint a click ran.
    DirectionOver,
    /// The second half of a sprint-off that a wall caused ([`Locomotion::end_sprint_at_wall`]).
    WallStunOff {
        index: u16,
    },
    Stance(bool),
    /// Starts [`Locomotion::deferred_roll`].
    StartDeferredRoll,
    /// The stun stats of a roll whose timeline runs one frame behind its `StartAction`.
    RollStunStats,
    /// The roll `roll` lets go of the player: a queued attack starts (a roll that is held into
    /// a sprint waits for the sprint's end).
    AttackWindow {
        roll: u64,
    },
    /// Re-issues the Move `StartAction` of a held click.
    MoveRestart {
        seq: u64,
    },
}

/// The movement component.
#[derive(Debug, Clone)]
pub struct Locomotion {
    /// The client's movement as the server models it: only what the server sends drives it.
    sim: ClientSim,
    /// The model played `SprintEnd` after the last frame.
    was_coasting: bool,
    state: MoveState,
    wasd: Cell,
    held: bool,
    /// A direction input arrived this frame: answer it.
    ack_wanted: bool,
    inbox: Vec<MoveInput>,
    due: Vec<(u64, Due)>,
    /// Buff index of the `must_sprint` buff while it is on.
    must_sprint_off: Option<u16>,
    combat_stance: bool,
    stance_scheduled: bool,
    /// The stance at entry has been set; later `AimMode` inputs are input-mode toggles.
    stance_entered: bool,
    /// Frame of an `AimMode(0)` before the entry stance (an area change sends 0 then 1).
    entry_mode0_frame: Option<u64>,
    /// The active weapon set.
    weapon_set: u8,
    /// An attack with a weapon swap ends the sprint after this frame's `MoveAck` (still
    /// scaled); `Some(streaming)`.
    attack_ends_sprint_after_ack: Option<bool>,
    /// The value of the last `0x200` not yet answered.
    stance_request: Option<u8>,
    /// The last `AimMode` asked for combat stance (mode 1).
    aim_combat: bool,
    teleport: Option<Cell>,
    /// A roll asked for while the previous roll's action still runs: it starts when that ends.
    queued_roll: Option<(UseSkill, Cell)>,
    /// Events that follow this frame's `MoveAck`.
    deferred: Vec<(Drain, MoveEvent)>,
    /// The sprint key is released in the batch being handled (set by `tick`).
    batch_release: bool,
    /// A roll whose key was already up when it was handled: acked now, started next frame.
    deferred_roll: Option<(UseSkill, Cell)>,
    /// The last attack asked for during a roll (acked already).
    queued_attack: Option<(UseSkill, Cell)>,
    /// The Move click being held.
    move_hold: Option<MoveHold>,
    hold_seq: u64,
    /// The sprint key was released before `must_sprint` went off: the sprint ends then.
    release_pending: bool,
    /// ... and it is `t0 + 35` now: end it after this frame's `MoveAck`.
    release_due: bool,
    /// An attack that starts after this frame's `MoveAck` (see `MustSprintOff`).
    attack_after_ack: Option<(UseSkill, Cell)>,
    /// A held click's Move that starts after this frame's `MoveAck` (see `MustSprintOff`).
    move_after_ack: Option<Cell>,
    /// A held click's Move runs during the sprint (started at or after `must_sprint` off): the
    /// sprint's end needs no `StartAction` of its own.
    sprint_move: bool,
    /// Frame of the latest Move `StartAction` answering a click input.
    move_start_frame: Option<u64>,
    /// The next roll started is one deferred by a click's Move: its timeline runs one frame
    /// behind its `StartAction` (see `on_use_skill`).
    roll_late: bool,
    /// The sprint that just ended by key release, see [`SprintEnd`].
    sprint_end: Option<SprintEnd>,
    /// The running `SprintEnd` of the model is a wall's `SprintEndBonk`.
    bonk: bool,
    /// The latest `0x63` was sent in click-to-move mode ([`MoveInput::CursorDirection`]).
    cursor_mode: bool,
    /// The cursor's offset from the player, as the roll key's target last said (`0x1a5`,
    /// `0x1a8`): the direction the client sends as `0x63` with Space held and no key.
    roll_cursor: Cell,
    /// The direction the entity last moved or aimed in: the client rolls along it when the
    /// roll has no target (cursor on the player) and no key is held.
    facing: Cell,
    /// The character's speed stats.
    speed_stats: SpeedStats,
    /// Set by [`Mover::set_speed_stats`], applied by the next tick.
    pending_speed_stats: Option<SpeedStats>,
    /// The sprint bonus the stat holds now.
    sprint_percent: i32,
}

impl Locomotion {
    /// An idle entity at `cell` with a character's movement facts.
    pub fn new(cell: Cell, config: &MovementConfig) -> Self {
        let mut sim = ClientSim::new(
            cell,
            config.walk_speed,
            config.curves.clone(),
            config.offset_scale,
        );
        sim.sprint_level = config.sprint_clearance_level;
        sim.sprint_turn_rate = config.sprint_turn_rate;
        sim.set_speed_stats(&config.speed_stats);
        Self {
            sim,
            was_coasting: false,
            state: MoveState::Idle,
            wasd: (0, 0),
            held: false,
            ack_wanted: false,
            inbox: Vec::new(),
            due: Vec::new(),
            must_sprint_off: None,
            combat_stance: false,
            stance_scheduled: false,
            stance_entered: false,
            entry_mode0_frame: None,
            weapon_set: 0,
            attack_ends_sprint_after_ack: None,
            stance_request: None,
            aim_combat: true,
            teleport: None,
            queued_roll: None,
            deferred: Vec::new(),
            batch_release: false,
            deferred_roll: None,
            queued_attack: None,
            move_hold: None,
            hold_seq: 0,
            attack_after_ack: None,
            move_after_ack: None,
            sprint_move: false,
            move_start_frame: None,
            roll_late: false,
            release_pending: false,
            release_due: false,
            sprint_end: None,
            bonk: false,
            cursor_mode: false,
            roll_cursor: (0, 0),
            speed_stats: config.speed_stats,
            pending_speed_stats: None,
            sprint_percent: 0,
            facing: (0, 0),
        }
    }

    /// Queues an input; it is handled in the next [`tick`](Self::tick).
    pub fn input(&mut self, input: MoveInput) {
        self.inbox.push(input);
    }

    /// Places the entity at `cell` in the next tick and stops it.
    pub fn teleport(&mut self, cell: Cell) {
        self.teleport = Some(cell);
    }

    pub fn state(&self) -> &MoveState {
        &self.state
    }

    /// The grid cell the entity stands in.
    pub fn cell(&self) -> Cell {
        self.sim.grid()
    }

    /// The client's own grid cell as the server models it.
    pub fn grid(&self) -> Cell {
        self.sim.grid()
    }

    /// Exact position in cells.
    pub fn position(&self) -> (f32, f32) {
        self.sim.point()
    }

    /// The client model (offline replays: it takes the client's logged plans).
    pub fn sim_mut(&mut self) -> &mut ClientSim {
        &mut self.sim
    }

    /// The latest WASD vector.
    pub fn wasd(&self) -> Cell {
        self.wasd
    }

    /// The sprint key is down.
    pub fn sprint_held(&self) -> bool {
        self.held
    }

    /// Sets the stance directly (a player whose entry stance is already done, e.g. restored
    /// or in tests); later `AimMode` inputs are then input-mode toggles.
    pub fn set_stance(&mut self, combat: bool) {
        self.combat_stance = combat;
        self.stance_entered = true;
    }

    /// Sets the active weapon set (restored state, tests).
    pub fn set_weapon_set(&mut self, set: u8) {
        self.weapon_set = set;
    }

    pub fn in_combat_stance(&self) -> bool {
        self.combat_stance
    }

    /// Waypoints left of a click-to-move walk (cells).
    pub fn path(&self) -> &[Cell] {
        self.sim.path()
    }

    pub fn sprint(&self) -> Option<&SprintRun> {
        match &self.state {
            MoveState::Rolling(r) => r.sprint.as_ref(),
            MoveState::Sprinting(s) => Some(s),
            _ => None,
        }
    }

    fn sprint_mut(&mut self) -> Option<&mut SprintRun> {
        match &mut self.state {
            MoveState::Rolling(r) => r.sprint.as_mut(),
            MoveState::Sprinting(s) => Some(s),
            _ => None,
        }
    }

    pub fn is_rolling(&self) -> bool {
        matches!(self.state, MoveState::Rolling(_))
    }

    /// One server frame; see the module docs for the order.
    /// A frame while the world is paused: only the stance timer runs (the official entry
    /// stance goes out while the client's post-load pause is on, five sessions).
    pub fn tick_paused(&mut self, ctx: &mut StepCtx) -> Vec<Keyed> {
        let mut out = Out::new();
        let (now, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.due)
            .into_iter()
            .partition(|(f, d)| *f <= ctx.frame && matches!(d, Due::Stance(_)));
        self.due = later;
        let mut unscaled = false;
        for (_, due) in now {
            self.fire(due, ctx, &mut out, &mut unscaled);
        }
        out.finish()
    }

    /// New speed stats arrived: the clients get the results before their Positioned tick
    /// (official `(0, 0, 0)`, 2026-11-02 f10902), the sprint's bonus included while it runs.
    fn apply_speed_stats(&mut self, out: &mut Out) {
        let Some(new) = self.pending_speed_stats.take() else {
            return;
        };
        let old = std::mem::replace(&mut self.speed_stats, new);
        let mut stats = Vec::new();
        if new.permyriad(self.sprint_percent) != old.permyriad(self.sprint_percent) {
            stats.push((MoveStat::MovementVelocityPermyriad, new.permyriad(self.sprint_percent)));
        }
        if new.action_speed_percent != old.action_speed_percent {
            stats.push((MoveStat::VirtualActionSpeed, new.action_speed_percent));
        }
        if new.velocity_cap != old.velocity_cap {
            stats.push((MoveStat::MovementVelocityCap, new.velocity_cap));
        }
        if !stats.is_empty() {
            out.key = Drain::Start;
            out.push(MoveEvent::Stats(stats));
        }
    }

    pub fn tick(&mut self, ctx: &mut StepCtx) -> Vec<Keyed> {
        let mut out = Out::new();
        self.apply_speed_stats(&mut out);
        // Order key 0: before the client's Positioned tick.
        let inbox = std::mem::take(&mut self.inbox);
        self.batch_release = inbox
            .iter()
            .any(|i| matches!(i, MoveInput::SprintInput { held: false }));
        let mut end_sprint = false;
        for input in inbox {
            out.key = Drain::Start;
            self.handle(input, ctx, &mut out, &mut end_sprint);
            self.apply_up_to(Drain::Start, &mut out, ctx);
        }
        // The components up to the Animated tick. A sprint that ended on a wall (in this Positioned
        // tick, or in the last frame's Actor tick) is answered at the wall's key.
        self.sim.positioned_tick(ctx.dt, ctx.terrain);
        if self.sim.take_bonked() && matches!(self.state, MoveState::Sprinting(_)) {
            out.key = AT_WALL;
            self.end_sprint_at_wall(ctx, &mut out);
        }
        if matches!(self.state, MoveState::Walking(Walk::Target { .. })) && !self.sim.has_move() {
            self.state = MoveState::Idle;
        }
        self.apply_up_to(Drain::at(1, 0), &mut out, ctx);
        self.sim.pathfinding_tick(ctx.dt, ctx.terrain);
        // The timeline: the model's clock tells what happens in this Animated tick.
        let mut unscaled_ack = false;
        if let MoveState::Rolling(_) = self.state {
            if self.sim.roll_event_due(ctx.dt) {
                self.fire_sprint_start(ctx, &mut out);
            }
            if self.sim.roll_end_due(ctx.dt) {
                self.fire_roll_end(ctx, &mut out);
            }
        }
        let (now, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.due)
            .into_iter()
            .partition(|(f, _)| *f <= ctx.frame);
        self.due = later;
        for (_, due) in now {
            self.fire(due, ctx, &mut out, &mut unscaled_ack);
        }
        self.apply_up_to(AT_LAST_BEFORE_ANIMATED, &mut out, ctx);
        self.sim.animated_tick(ctx.dt, ctx.terrain);
        // The client's `SprintEnd` is over: its Actor tick walks on a held key by itself.
        if self.was_coasting && !self.sim.is_coasting() {
            let bonk = std::mem::take(&mut self.bonk);
            if self.wasd != (0, 0)
                && (!self.cursor_mode || bonk)
                && matches!(self.state, MoveState::Idle)
            {
                self.sprint_end = None;
                self.resume_walking();
            }
        }
        // Keyed to the Actor: the `0x64` with the grid after the Positioned tick, the starts of
        // the server's own, the sprint's end.
        if let Some(cell) = self.teleport.take() {
            self.state = MoveState::Idle;
            self.wasd = (0, 0);
            self.ack_wanted = false;
            out.push(MoveEvent::Teleport { cell });
        }
        // The key's release and the zero vector in one batch: the sprint-off goes first, without
        // a `0x64`, whose zero comes in the next frame (2026-11-03 f5253).
        end_sprint |= std::mem::take(&mut self.release_due);
        let zero_after_off = end_sprint && self.wasd == (0, 0) && self.ack_wanted && self.sprint().is_some();
        if zero_after_off {
            self.ack_wanted = false;
            self.due.push((ctx.frame + 1, Due::UnscaledAck));
        } else {
            self.move_ack(unscaled_ack, ctx, &mut out);
        }
        for (key, event) in std::mem::take(&mut self.deferred) {
            out.push_at(key, event);
        }
        if let Some(streaming) = self.attack_ends_sprint_after_ack.take() {
            // Official f11798: `0x64 (0, 180)` unscaled at r+1.
            if streaming && self.wasd != (0, 0) {
                self.due.push((ctx.frame + 1, Due::UnscaledAck));
            }
            self.finish_sprint();
        }
        if let Some(target) = self.move_after_ack.take() {
            let action = Self::alloc_action(ctx);
            out.push_at(
                AT_SPRINT_END_MOVE,
                MoveEvent::ActionStart {
                    kind: ActionKind::Move,
                    skill: ctx.config.skills.move_skill,
                    flags: ACTION_INPUT_FLAG,
                    action,
                    cell: self.sim.grid(),
                    target,
                    entity: None,
                    unk: 0xff,
                },
            );
        }
        if let Some((req, target)) = self.attack_after_ack.take() {
            out.key = AT_SPRINT_END_MOVE;
            self.start_attack(req, target, false, ctx, &mut out);
            for (key, event) in std::mem::take(&mut self.deferred) {
                out.push_at(key, event);
            }
        }
        if end_sprint {
            let no_ack = !out.items.iter().any(|k| matches!(k.event, MoveEvent::MoveAck { .. }));
            self.end_sprint(no_ack, ctx, &mut out);
        }
        self.apply_up_to(AT_LAST_BEFORE_ACTOR, &mut out, ctx);
        self.sim.actor_tick(ctx.dt, ctx.terrain);
        self.was_coasting = self.sim.is_coasting();
        out.finish()
    }

    /// Runs the frame's messages up to `pos` on the model, in the client's order.
    fn apply_up_to(&mut self, pos: Drain, out: &mut Out, ctx: &StepCtx) {
        let mut order: Vec<usize> = (0..out.items.len())
            .filter(|&i| !out.applied[i] && out.items[i].key <= pos)
            .collect();
        order.sort_by_key(|&i| out.items[i].key);
        for i in order {
            out.applied[i] = true;
            let event = out.items[i].event.clone();
            self.apply_event(&event, ctx);
        }
    }

    fn apply_event(&mut self, event: &MoveEvent, ctx: &StepCtx) {
        let cfg = ctx.config;
        let t = ctx.terrain;
        match *event {
            MoveEvent::MoveAck { dir, cell } => self.sim.on_move_ack(dir, cell, t),
            // The two `0x64` of a teleport (`Codec`).
            MoveEvent::Teleport { cell } => {
                self.sim.on_move_ack((1, 0), cell, t);
                self.sim.on_move_ack((0, 0), cell, t);
            }
            MoveEvent::SprintState { held } => self.sim.on_sprint_key(held),
            MoveEvent::ActionStart {
                kind, cell, target, ..
            } => match kind {
                ActionKind::Roll => self.sim.on_start_roll(cell, target, cfg.roll_travel_distance, t),
                ActionKind::Move => self.sim.on_start_move(cell, target, t),
                ActionKind::Other => self.sim.on_start_other(cell, t),
            },
            MoveEvent::Stats(ref stats) => {
                for &(stat, value) in stats {
                    match stat {
                        MoveStat::IsSprinting => self.sim.on_is_sprinting(value != 0),
                        MoveStat::MustSprint => self.sim.on_must_sprint(value != 0),
                        MoveStat::MovementVelocityPermyriad => self.sim.on_permyriad(value),
                        MoveStat::VirtualActionSpeed => self.sim.on_action_speed(value),
                        MoveStat::MovementVelocityCap => self.sim.on_velocity_cap(value),
                        _ => {}
                    }
                }
            }
            MoveEvent::TargetAck { skill, x, y } => {
                if cfg.skills.is_roll(skill) {
                    self.sim.on_skill_target((x, y));
                }
            }
            _ => {}
        }
    }

    // ---- inputs

    fn handle(
        &mut self,
        input: MoveInput,
        ctx: &mut StepCtx,
        out: &mut Out,
        end_sprint: &mut bool,
    ) {
        match input {
            MoveInput::Direction { x, y } => self.on_direction((x, y), false, ctx),
            MoveInput::CursorDirection { x, y } => self.on_direction((x, y), true, ctx),
            MoveInput::UseSkill(req) => self.on_use_skill(req, ctx, out),
            MoveInput::UpdateSkillTarget { x, y, skill } => {
                let (cx, cy) = self.sim.grid();
                let abs = (cx + x, cy + y);
                self.steer(skill, (x, y), abs, ctx);
                out.push(MoveEvent::TargetAck {
                    skill,
                    x: abs.0,
                    y: abs.1,
                });
            }
            MoveInput::SkillInputEnd => out.push(MoveEvent::InputEndAck),
            MoveInput::SkillReleased { skill, flag } => {
                if skill == ctx.config.skills.move_skill {
                    self.move_hold = None;
                }
                // In click-to-move mode the roll key is released with `SkillReleased` (no
                // `SkillInputEnd`), which is answered with both acks (2026-10-20 capture).
                if ctx.config.skills.is_roll(skill) {
                    out.push(MoveEvent::InputEndAck);
                }
                out.push(MoveEvent::ReleasedAck { skill, flag });
            }
            MoveInput::SprintInput { held } => {
                self.held = held;
                if let MoveState::Rolling(r) = &mut self.state {
                    r.held = held;
                    // Once released the client's roll keeps the tap curve even if the key comes
                    // back before `t0 + 15` (private f1065: released, pressed again at +6).
                    r.released |= !held;
                }
                out.push(MoveEvent::SprintState { held });
                if !held && let Some(run) = self.sprint().copied() {
                    // A sprint cannot end before `must_sprint` goes off at t0 + 35: a key
                    // released earlier (2026-10-20 capture: at t0 + 19) is answered at once
                    // (`1b1`, `2e7`) and the sprint ends at t0 + 35.
                    let end = run.start_frame + MUST_SPRINT_END_FRAMES;
                    if ctx.frame < end {
                        self.release_pending = true;
                    } else {
                        *end_sprint = true;
                    }
                } else if held && self.sprint().is_some() {
                    // Pressed again before t0 + 35: the earlier release no longer ends the sprint
                    // (2026-11-02 f2178: released at t0 + 22, pressed at t0 + 30, the roll press
                    // only acked, the sprint runs on to the next release).
                    self.release_pending = false;
                }
            }
            MoveInput::Aim { x, y } => {
                // Never echoed while rolling or sprinting (all sessions).
                if !self.is_rolling() && self.sprint().is_none() && self.deferred_roll.is_none() {
                    out.push(MoveEvent::AimEcho { cell: (x, y) });
                }
            }
            MoveInput::AimEntity { .. } => {}
            MoveInput::SprintOption { continues } => self.sim.continues_with_move_input = continues,
            MoveInput::StanceRequest { value } => self.stance_request = Some(value),
            // The client cannot use a skill or swap again until this answer (session +0x7a90,
            // cleared by 0xff), so every request is answered. Whether a swap during an action
            // or a walk changes it is not captured (gap #7).
            MoveInput::WeaponSwap { set } => {
                self.weapon_set = set;
                out.push(MoveEvent::WeaponSet {
                    set,
                    requested: true,
                })
            }
            MoveInput::AimMode { mode } => {
                let combat = mode == 1;
                self.aim_combat = combat;
                if !self.stance_entered {
                    // At entry the stance follows the first mode 1 in the batch 8 frames after
                    // the one answering it (captures: 9 frames after the input's frame; 14 in the
                    // 2026-10-20 session: it depends on the area load). After an area change
                    // (mode 0 then 1 in one batch) 3 frames.
                    if !combat {
                        self.entry_mode0_frame = Some(ctx.frame);
                    } else if !self.stance_scheduled {
                        self.stance_scheduled = true;
                        let delay = if self.entry_mode0_frame == Some(ctx.frame) {
                            ctx.config.area_stance_delay_frames
                        } else {
                            ctx.config.stance_delay_frames
                        };
                        self.due.push((ctx.frame + delay as u64, Due::Stance(true)));
                    }
                } else if combat != self.combat_stance || self.stance_request.is_some() {
                    // Input mode toggled (with the stance request `0x200`): 2 of 2 in the
                    // 2026-10-20 capture, in the batch answering the input, in both directions,
                    // while standing.
                    // UNVERIFIED (gap #7): while walking or rolling.
                    self.due.push((
                        ctx.frame + STANCE_TOGGLE_FRAMES,
                        Due::Stance(combat),
                    ));
                }
            }
        }
    }

    fn on_direction(&mut self, dir: Cell, cursor: bool, ctx: &StepCtx) {
        let window = RELEASE_WINDOW_FRAMES;
        let click_end = self
            .sprint_end
            .is_some_and(|e| e.click && ctx.frame <= e.frame + window);
        if (cursor && matches!(self.state, MoveState::Walking(Walk::Target { .. })))
            || (click_end && (cursor || dir == (0, 0)))
        {
            // A held click walks, or its Move ran the sprint that just ended: cursor vectors
            // and the client's zero vector are not answered (2026-10-20 f3644,
            // 2026-10-20_21-51-49 f8952, 2026-11-02 f7911).
            return;
        }
        self.wasd = dir;
        self.cursor_mode = cursor;
        self.ack_wanted = true;
        let moving = dir != (0, 0);
        if (!cursor || self.bonk) && self.sim.is_coasting() {
            // The client's `SprintEnd` animation still plays: `Actor_StartWasdWalk` starts no
            // walk and the coast goes on; the key, if still down at its end, walks on then.
            if moving {
                self.facing = dir;
            }
            return;
        }
        if moving {
            self.facing = dir;
            // Cursor vectors still in flight when the sprint ended keep its direction up to
            // date; a key vector is a real new walk. In click-to-move mode every vector is a
            // cursor one, for a limited time (a sprint that coasts at once ignores them).
            let cursor_vector = cursor || self.is_cursor_vector(dir);
            match &mut self.sprint_end {
                Some(end) if end.coasting => {
                    if cursor && ctx.frame <= end.frame + window {
                        end.dir = dir;
                        return;
                    }
                    self.sprint_end = None;
                }
                Some(end) if cursor_vector => end.dir = dir,
                _ => self.sprint_end = None,
            }
        } else if let Some(end) = self.sprint_end.take()
            && ctx.frame <= end.frame + RELEASE_WINDOW_FRAMES
            && end.cursor
            && matches!(self.state, MoveState::Walking(Walk::Direction(_)))
        {
            // The sprint followed the cursor and the keys were already up when the unscaled
            // `MoveAck` of its end reached the client: that never restarted its walk, it
            // coasts (see `end_sprint`). Carry on from the step the client is at.
            self.state = MoveState::Idle;
            return;
        }
        if !moving && self.sim.is_coasting() {
            // The client's walk is over already; a stop does not cut the coasting short.
            return;
        }
        if let Some(run) = self.sprint_mut() {
            // The stream starts with the first vector after the sprint started. A zero vector
            // is answered once. The official client never sends one while a sprint runs (it
            // sends the cursor direction); UNVERIFIED (gap #4) what the stream does if it did.
            run.streaming = moving;
        }
        match self.state {
            MoveState::Rolling(_) | MoveState::Sprinting(_) => {}
            MoveState::Idle | MoveState::Walking(Walk::Direction(_)) => {
                if moving {
                    self.state = MoveState::Walking(Walk::Direction(dir));
                } else {
                    self.state = MoveState::Idle;
                }
            }
            MoveState::Walking(Walk::Target { .. }) => {
                // UNVERIFIED (gap #2): WASD takes over a click walk; a stop leaves it running.
                if moving {
                    self.state = MoveState::Walking(Walk::Direction(dir));
                }
            }
        }
    }

    fn target_of(&self, req: &UseSkill) -> Cell {
        match req.entity {
            Some(_) => (req.x, req.y),
            None => (self.sim.grid().0 + req.x, self.sim.grid().1 + req.y),
        }
    }

    fn skill_ack(req: &UseSkill, target: Cell) -> MoveEvent {
        MoveEvent::SkillAck {
            skill: req.skill,
            flags: req.flags,
            unk: req.unk,
            target_flags: req.target_flags,
            entity: req.entity,
            x: target.0,
            y: target.1,
        }
    }

    fn on_use_skill(&mut self, req: UseSkill, ctx: &mut StepCtx, out: &mut Out) {
        let target = self.target_of(&req);
        let busy = matches!(self.state, MoveState::Rolling(_) | MoveState::Sprinting(_));
        if ctx.config.skills.is_roll(req.skill) {
            match self.state {
                // Chained rolls (2026-10-20: presses 28 and 29 frames after the previous start,
                // plus the 2026-10-14 case at 27): acked at once, the roll itself starts when
                // the previous action ends (`max(r+1, P+30)`). A press 9 frames in that was
                // released again was dropped: this keeps the last press (single slot);
                // UNVERIFIED (gap #3) whether a released key drops it.
                MoveState::Rolling(_) if req.target_flags & UseSkill::HELD_REPEAT == 0 => {
                    out.push(Self::skill_ack(&req, target));
                    self.queued_roll = Some((req, target));
                }
                // Held-key repeats of the roll skill (target flags 0x13) are acked and nothing
                // else happens. UNVERIFIED (gap #3): a roll while sprinting is treated alike.
                MoveState::Rolling(_) | MoveState::Sprinting(_) => {
                    out.push(Self::skill_ack(&req, target));
                }
                // A held repeat never starts a roll: after an attack ended the sprint with the
                // key still down, the repeats are only acked (2026-11-02, f11801, f12757).
                _ if req.target_flags & UseSkill::HELD_REPEAT != 0 => {
                    out.push(Self::skill_ack(&req, target));
                }
                // Press and release handled together (inputs queued while paused): the
                // answers come in input order and the roll itself one batch later (2026-10-20
                // capture, scheduled headers `(5, 8)`, `(5, 17)`, `(5, 25)`).
                // A Move started by a click input of this batch: the roll is acked now and its
                // `StartAction` comes one batch later, its stun stats and whole timeline (sprint,
                // tap end, roll end, `must_sprint` off) one more frame behind (2026-10-20
                // f3712, f4485; 2026-11-02 f8037, f8075).
                _ if self.move_start_frame == Some(ctx.frame) => {
                    out.push(Self::skill_ack(&req, target));
                    self.deferred_roll = Some((req, target));
                    self.roll_late = true;
                    self.due.push((ctx.frame + 1, Due::StartDeferredRoll));
                }
                _ if self.batch_release && self.held => {
                    out.push(Self::skill_ack(&req, target));
                    self.deferred_roll = Some((req, target));
                    self.due.push((ctx.frame + 1, Due::StartDeferredRoll));
                }
                _ => self.start_roll(req, target, true, ctx, out),
            }
        } else if req.skill == ctx.config.skills.move_skill {
            if busy {
                // Verified (2026-10-20, 1 roll and 3 sprints): acked at once, nothing moves;
                // the held click's walk resumes with a `StartAction` when the roll ends.
                // Once `must_sprint` is off a running sprint follows the click: every press
                // or repeat starts a Move (flags + 0x10, before its ack) and the sprint goes
                // on (8 of 8 sprints with a held click past t0 + 35, 2026-10-20 and 2026-11-02).
                self.note_hold(&req, target, ctx);
                if matches!(self.state, MoveState::Sprinting(_)) && self.must_sprint_off.is_none()
                {
                    self.sprint_move = true;
                    out.push(MoveEvent::ActionStart {
                        kind: ActionKind::Move,
                        skill: req.skill,
                        flags: req.flags | ACTION_INPUT_FLAG,
                        action: Self::alloc_action(ctx),
                        cell: self.sim.grid(),
                        target,
                        entity: req.entity,
                        unk: req.unk,
                    });
                }
                out.push(Self::skill_ack(&req, target));
            } else if self.in_click_sprint_end() {
                // The client plays `SprintEnd` after a sprint its click ran: acked only.
                self.note_hold(&req, target, ctx);
                out.push(Self::skill_ack(&req, target));
            } else {
                self.start_walk(req, target, ctx, out);
            }
        } else {
            self.start_other(req, target, ctx, out);
        }
    }

    fn alloc_action(ctx: &mut StepCtx) -> u16 {
        let id = *ctx.next_action;
        *ctx.next_action = ctx.next_action.wrapping_add(1);
        id
    }

    /// Starts a roll towards the absolute `target`. `ack` is false for a queued roll, whose
    /// `UseSkillAck` was sent when it was asked for.
    fn start_roll(
        &mut self,
        req: UseSkill,
        target: Cell,
        ack: bool,
        ctx: &mut StepCtx,
        out: &mut Out,
    ) {
        let cell = self.sim.grid();
        let offset = (target.0 - cell.0, target.1 - cell.1);
        let action = Self::alloc_action(ctx);
        // A roll answering an input runs at order key 0, before this frame's Animated tick; a
        // queued one, or one held back by a release in its batch, at `(player, 5, 8)`, also before
        // that tick (all captures: 21 of 21, its tail at `(5, 17)`, stun at `(5, 25)`). One held back
        // by a click's Move runs after the tick at `(9, 1)`, so its clip and whole timeline start
        // one frame later, stun at `(5, 6)` (8 of 8, 2026-10-20 f3712, f4485; 2026-11-02 f8037, f8075).
        self.roll_late = false;
        let start_key = out.key;
        let late = start_key == AT_RESTART;
        let scheduled = start_key == AT_ROLL_END_MOVE;
        let tail_key = if late {
            AT_SPRINT_END_LATE
        } else if scheduled {
            Drain::at(5, 17)
        } else {
            Drain::Start
        };
        // Verified (2026-10-20): a standing roll gets the same batch and ends in Idle with no
        // `MoveAck`. The client rolls along the WASD vector when a key is held, else towards
        // the cursor (`target`, steered every 2 frames); `target` is the cursor either way.
        // The client's roll heading (`0x141b76da0` neighbourhood, `0x141b8bda0`): the held input
        // vector, else the direction to the action's target, and the entity's facing when the
        // target is the player's own cell.
        self.roll_cursor = offset;
        let wasd_driven = self.wasd != (0, 0);
        let heading = if wasd_driven {
            self.wasd
        } else if offset != (0, 0) {
            offset
        } else {
            self.facing
        };
        if heading != (0, 0) {
            self.facing = heading;
        }
        self.sprint_move = false;
        self.sprint_end = None;
        self.bonk = false;
        let t0 = ctx.frame + late as u64;
        self.state = MoveState::Rolling(Rolling {
            start_frame: t0,
            held: self.held,
            offset,
            wasd_driven,
            action,
            sprint: None,
            sprinted: false,
            released: !self.held,
        });
        out.push_at(start_key, MoveEvent::ActionStart {
            kind: ActionKind::Roll,
            skill: req.skill,
            flags: req.flags | ACTION_INPUT_FLAG,
            action,
            cell,
            target,
            entity: req.entity,
            unk: req.unk,
        });
        out.push_at(tail_key, MoveEvent::ActionEvent);
        out.push_at(tail_key, MoveEvent::Stats(vec![(MoveStat::IsDodgeRolling, 1)]));
        if ack {
            out.push_at(tail_key, Self::skill_ack(&req, target));
        }
        if late {
            self.due.push((t0, Due::RollStunStats));
        } else if scheduled {
            out.push_at(Drain::at(5, 25), MoveEvent::Stats(stun_stats(ROLL_STUN_THRESHOLD)));
        } else {
            out.push_at(AT_ROLL_STUN, MoveEvent::Stats(stun_stats(ROLL_STUN_THRESHOLD)));
        }
        self.due
            .push((t0 + TAP_END_FRAMES, Due::RollEndTap { roll: t0 }));
    }

    /// Click-to-move, verified on 2026-10-20 (6 single clicks, 4 holds): the press is answered
    /// with `StartAction` Move (request flags + 0x10, start = the server's cell, target =
    /// cell + offset) and a `UseSkillAck`; every held repeat (target flags 0x10) the same with
    /// flags 0x10, so a repeat re-aims the walk; the release with a `SkillReleasedAck`.
    fn start_walk(
        &mut self,
        req: UseSkill,
        target: Cell,
        ctx: &mut StepCtx,
        out: &mut Out,
    ) {
        let cell = self.sim.grid();
        if target != cell {
            self.facing = (target.0 - cell.0, target.1 - cell.1);
        }
        self.sprint_end = None;
        self.bonk = false;
        self.state = MoveState::Walking(Walk::Target { goal: target });
        self.note_hold(&req, target, ctx);
        self.move_start_frame = Some(ctx.frame);
        out.push(MoveEvent::ActionStart {
            kind: ActionKind::Move,
            skill: req.skill,
            flags: req.flags | ACTION_INPUT_FLAG,
            action: Self::alloc_action(ctx),
            cell,
            target,
            entity: req.entity,
            unk: req.unk,
        });
        out.push(Self::skill_ack(&req, target));
    }

    /// The client plays `SprintEnd` after a sprint a held click took part in: Move requests
    /// are only acked and no restart comes.
    fn in_click_sprint_end(&self) -> bool {
        self.sim.is_coasting() && self.sprint_end.is_some_and(|e| e.click)
    }

    /// The held click, if its input is recent.
    fn live_hold(&self, frame: u64) -> Option<MoveHold> {
        self.move_hold
            .filter(|h| frame - h.last_input <= MoveHold::STALE_FRAMES)
    }

    /// Remembers the click as held until its release; every press or repeat restarts the
    /// restart timer.
    fn note_hold(&mut self, req: &UseSkill, target: Cell, ctx: &StepCtx) {
        let repeat = req.target_flags & UseSkill::HELD_REPEAT != 0;
        match &mut self.move_hold {
            Some(h) if repeat => {
                h.history = [(ctx.frame, target), h.history[0], h.history[1]];
                h.goal = target;
                h.flags = req.flags;
                h.last_input = ctx.frame;
                h.last_start = ctx.frame;
            }
            _ => {
                self.hold_seq += 1;
                self.move_hold = Some(MoveHold {
                    goal: target,
                    flags: req.flags,
                    seq: self.hold_seq,
                    last_input: ctx.frame,
                    last_start: ctx.frame,
                    history: [(ctx.frame, target); 3],
                });
            }
        }
        self.due.push((
            ctx.frame + CLICK_RESTART_FRAMES,
            Due::MoveRestart { seq: self.hold_seq },
        ));
    }

    fn start_other(
        &mut self,
        req: UseSkill,
        target: Cell,
        ctx: &mut StepCtx,
        out: &mut Out,
    ) {
        // An attack during a roll is acked at once and the last one asked for starts when the
        // roll lets go: `attack_cancel_frames` after the roll started (the roll is cut short
        // there), or, when Space was held past +15, with the end of the sprint at +35 (2026-10-20
        // capture: 3 of 3 and 1 of 1). Attacks end a running sprint; they do not stop walking but
        // slow it (`update_speed_factor`; UNVERIFIED (gap #6) for a plain WASD walk).
        if let MoveState::Rolling(r) = self.state {
            out.push(Self::skill_ack(&req, target));
            // A held repeat does not replace the request (the attack on a hovered monster
            // kept its entity target in the capture).
            if req.target_flags & UseSkill::HELD_REPEAT == 0 || self.queued_attack.is_none() {
                self.queued_attack = Some((req, target));
            }
            let at =
                (r.start_frame + ATTACK_CANCEL_FRAMES).max(ctx.frame + 1);
            self.due.push((
                at,
                Due::AttackWindow {
                    roll: r.start_frame,
                },
            ));
            return;
        }
        self.start_attack(req, target, true, ctx, out);
    }


    /// Starts an attack now; `ack` is false for a queued one, whose ack went out when it was
    /// asked for.
    fn start_attack(
        &mut self,
        req: UseSkill,
        target: Cell,
        ack: bool,
        ctx: &mut StepCtx,
        out: &mut Out,
    ) {
        let cell = self.sim.grid();
        // The attack replaces whatever was waiting (seg1 f3746, a press only acked, then f3755).
        self.queued_roll = None;
        out.push(MoveEvent::ActionStart {
            kind: ActionKind::Other,
            skill: req.skill,
            flags: req.flags | ACTION_INPUT_FLAG,
            action: Self::alloc_action(ctx),
            cell,
            target,
            entity: req.entity,
            unk: req.unk,
        });
        // Official seqs of the sprint's end by an attack: at key 0 the first half, then the second at
        // `(player, 9, 9)`; started after the `0x64` `(9, 4)`, `(9, 7)`, `(9, 12)`, `(9, 16)`;
        // with a swap `(9, 10)`, `(9, 14)`, `(9, 17)`.
        let in_input = out.key == Drain::Start;
        // A skill of the other weapon set: the server swaps by itself (2026-11-02, f5464 and
        // f11798): the ack at once, then after the frame's `MoveAck` the `0x1d5`, the sprint-off
        // (the `MoveAck` was still scaled; the unscaled one follows at r+1) and the `0x1ca`.
        if let Some(set) = SkillIds::weapon_set_of(req.skill).filter(|s| *s != self.weapon_set) {
            self.weapon_set = set;
            if ack {
                out.push(Self::skill_ack(&req, target));
            }
            self.deferred.push((Drain::at(9, 2), MoveEvent::WeaponSet {
                set,
                requested: false,
            }));
            if let Some(run) = self.sprint().copied() {
                let (off, ev, tail) = (Drain::at(9, 10), Drain::at(9, 14), Drain::at(9, 17));
                if let Some(n) = self.must_sprint_off.take() {
                    self.deferred.push((off, MoveEvent::BuffRemoved { index: n }));
                    self.deferred
                        .push((off, MoveEvent::Stats(vec![(MoveStat::MustSprint, 0)])));
                }
                self.deferred.push((off, MoveEvent::BuffRemoved {
                    index: run.buff_base + 1,
                }));
                let off_stats = self.sprint_off_stats();
                self.deferred.push((off, MoveEvent::Stats(off_stats)));
                self.deferred.push((ev, MoveEvent::ActionEvent));
                self.deferred.push((tail, MoveEvent::BuffRemoved {
                    index: run.buff_base + 2,
                }));
                self.deferred
                    .push((tail, MoveEvent::Stats(vec![(MoveStat::AllHitsStunYou, 0)])));
                self.attack_ends_sprint_after_ack = Some(run.streaming);
            } else {
                self.deferred.push((Drain::at(9, 14), MoveEvent::ActionEvent));
            }
            return;
        }
        // Every action start but Move's has a `0x1ca` (all eight sessions: 0x80400040 147 of
        // 147, 0x2000c266 31 of 31, 0x85000040 47 of 47; skills with their own stats or buffs
        // send those first), then the ack.
        let Some(run) = self.sprint().copied() else {
            out.push(MoveEvent::ActionEvent);
            if ack {
                out.push(Self::skill_ack(&req, target));
            }
            return;
        };
        // The attack ends the sprint: (`must_sprint` off, if it still runs,) `0x1c8`, the first
        // half of the sprint-off, `0x1ca`, the ack, then after the frame's `MoveAck` (unscaled)
        // the second half.
        if let Some(n) = self.must_sprint_off.take() {
            let start = out.items.pop().expect("ActionStart");
            out.applied.pop();
            out.push(MoveEvent::BuffRemoved { index: n });
            out.push(MoveEvent::Stats(vec![(MoveStat::MustSprint, 0)]));
            out.push_at(start.key, start.event);
        }
        let (off, ev, tail) = if in_input {
            (Drain::Start, Drain::Start, Drain::at(9, 9))
        } else {
            (Drain::at(9, 7), Drain::at(9, 12), Drain::at(9, 16))
        };
        out.push_at(off, MoveEvent::BuffRemoved {
            index: run.buff_base + 1,
        });
        out.push_at(off, MoveEvent::Stats(self.sprint_off_stats()));
        out.push_at(ev, MoveEvent::ActionEvent);
        if ack {
            out.push_at(ev, Self::skill_ack(&req, target));
        }
        out.push_at(tail, MoveEvent::BuffRemoved {
            index: run.buff_base + 2,
        });
        out.push_at(tail, MoveEvent::Stats(vec![(MoveStat::AllHitsStunYou, 0)]));
        self.ack_wanted |= run.streaming && self.wasd != (0, 0);
        self.finish_sprint();
    }

    /// Re-aims the running action after a `UpdateSkillTarget`.
    fn steer(&mut self, skill: u32, offset: Cell, abs: Cell, ctx: &StepCtx) {
        if ctx.config.skills.is_roll(skill) {
            self.roll_cursor = offset;
        }
        match &mut self.state {
            MoveState::Rolling(r) if ctx.config.skills.is_roll(skill) => {
                if offset != (0, 0) && !r.wasd_driven {
                    r.offset = offset;
                }
            }
            MoveState::Walking(Walk::Target { goal }) if skill == ctx.config.skills.move_skill => {
                *goal = abs;
            }
            _ => {}
        }
    }

    // ---- timeline

    /// The roll's clip reaches `sprint_start` in this frame's Animated tick: the server's sprint
    /// starts if the key was held all along. A roll released before that never sprints, even with
    /// the key down again (official f12190): the press is queued and the next roll sprints.
    fn fire_sprint_start(&mut self, ctx: &mut StepCtx, out: &mut Out) {
        let cfg = ctx.config;
        let MoveState::Rolling(r) = &mut self.state else {
            return;
        };
        if r.released || r.sprint.is_some() {
            return;
        }
        let roll = r.start_frame;
        let n = *ctx.next_buff;
        *ctx.next_buff += 3;
        // A vector handled in the frame the sprint starts opens the stream already
        // (2026-10-14_22-23-02 f308: scaled in the sprint-start batch).
        let streaming = self.ack_wanted && self.wasd != (0, 0);
        r.sprint = Some(SprintRun {
            start_frame: roll,
            buff_base: n,
            streaming,
        });
        r.sprinted = true;
        out.key = AT_ANIMATED;
        for (i, row) in cfg.sprint_buff_rows.iter().enumerate() {
            out.push(MoveEvent::BuffAdded {
                index: n + i as u16,
                row: *row,
                fade: None,
            });
        }
        let mut stats = vec![
            (MoveStat::SprintBlendTimeOverrideMs, SPRINT_BLEND_OVERRIDE_MS),
            (MoveStat::IsSprinting, 1),
            (MoveStat::MustSprint, 1),
            (MoveStat::AllHitsStunYou, 1),
        ];
        // Official: only while nothing else changes the speed (2026-11-02 f11215, 8 of 8).
        if self.speed_stats.is_plain() {
            stats.push((MoveStat::VirtualSkillIgnoreNamelockingDynamically, 1));
        }
        out.push(MoveEvent::Stats(stats));
        self.must_sprint_off = Some(n);
        self.due
            .push((roll + MUST_SPRINT_END_FRAMES, Due::MustSprintOff));
    }

    /// The roll's clip ends in this frame's Animated tick.
    fn fire_roll_end(&mut self, ctx: &mut StepCtx, out: &mut Out) {
        let cfg = ctx.config;
        let MoveState::Rolling(r) = self.state else {
            return;
        };
        out.key = AT_ANIMATED;
        // A tap roll cleared its stats earlier; one that sprinted does it now.
        if r.sprinted {
            roll_end_stats(out);
        }
        if let Some(run) = r.sprint {
            let percent = cfg.sprint_bonus_percent.unwrap_or(0.0) as i32;
            self.sprint_percent = percent;
            let speed = self.speed_stats;
            let mut stats = vec![(MoveStat::MovementVelocityPermyriad, speed.permyriad(percent))];
            if speed.is_plain() {
                stats.insert(0, (MoveStat::BaseMoveSpeedFromSprint, percent));
            }
            out.push(MoveEvent::Stats(stats));
            out.push(MoveEvent::BuffAdded {
                index: run.buff_base + 1,
                row: cfg.sprint_buff_rows[1],
                fade: None,
            });
            out.push(MoveEvent::BuffAdded {
                index: run.buff_base,
                row: cfg.sprint_buff_rows[0],
                fade: Some(SPRINT_BUFF_FADE_SECS),
            });
            self.state = MoveState::Sprinting(run);
            // A press during a roll that became a sprint starts nothing: the client's Move runs
            // on (private f1071: DodgeRoll ends at f1095, Move runs to f2560), and a new roll here
            // would drop the sprint without its sprint-off stats, leaving the client sprinting
            // for good.
            self.queued_roll = None;
        } else if let Some(h) = self.live_hold(ctx.frame)
            && self.queued_roll.is_none()
        {
            // A held click (unless a queued roll starts now: 2026-10-20 f3744): the walk resumes
            // with a `StartAction` towards the latest target.
            self.state = MoveState::Walking(Walk::Target { goal: h.goal });
            // This start counts for the restart timer (seg1 f3429: none at f3430).
            if let Some(m) = &mut self.move_hold {
                m.last_start = ctx.frame;
            }
            self.due.push((ctx.frame + CLICK_RESTART_FRAMES, Due::MoveRestart { seq: h.seq }));
            let action = Self::alloc_action(ctx);
            out.push_at(
                AT_ROLL_END_MOVE,
                MoveEvent::ActionStart {
                    kind: ActionKind::Move,
                    skill: cfg.skills.move_skill,
                    flags: h.flags & !ACTION_INPUT_FLAG,
                    action,
                    cell: self.sim.grid(),
                    target: h.goal,
                    entity: None,
                    unk: 0xff,
                },
            );
        } else {
            // The client resumes walking by itself after a roll; no message.
            self.resume_walking();
        }
        if let Some((req, target)) = self.queued_roll.take() {
            out.key = AT_ROLL_END_MOVE;
            self.start_roll(req, target, false, ctx, out);
        }
    }

    fn fire(&mut self, due: Due, ctx: &mut StepCtx, out: &mut Out, unscaled: &mut bool) {
        let cfg = ctx.config;
        match due {
            Due::RollEndTap { roll } => {
                if let MoveState::Rolling(r) = self.state
                    && r.start_frame == roll
                    && !r.sprinted
                {
                    // `is_dodge_rolling` goes off; the action itself runs on to the clip's end.
                    out.key = AT_ANIMATED;
                    roll_end_stats(out);
                }
            }
            Due::MustSprintOff => {
                if let Some(n) = self.must_sprint_off.take() {
                    out.key = AT_BUFFS;
                    out.push(MoveEvent::BuffRemoved { index: n });
                    out.push(MoveEvent::Stats(vec![(MoveStat::MustSprint, 0)]));
                }
                // An attack asked for during the roll ends the sprint now, after this frame's
                // `MoveAck` (official order: `must_sprint` off, `MoveAck`, `StartAction`, ...).
                if self.sprint().is_some() {
                    // A held click takes the sprint over: a Move towards its goal after the
                    // `MoveAck` (official order: `must_sprint` off, `MoveAck`, `StartAction`
                    // flags 0x10), the sprint goes on (2026-10-20 f3779, 2026-11-02 f7746).
                    if let Some(h) = self.live_hold(ctx.frame)
                        && matches!(self.state, MoveState::Sprinting(_))
                        && !self.release_pending
                    {
                        self.move_after_ack = Some(h.settled_goal(ctx.frame, 0));
                        self.sprint_move = true;
                    }
                    self.attack_after_ack = self.queued_attack.take();
                    if self.release_pending {
                        self.release_pending = false;
                        self.release_due = true;
                    }
                }
            }
            Due::AttackWindow { roll } => {
                let MoveState::Rolling(r) = self.state else {
                    return;
                };
                // A roll that sprinted hands the attack to the sprint's end (see
                // `MustSprintOff`) or to its own end.
                if r.start_frame != roll || r.sprinted {
                    return;
                }
                if let Some((req, target)) = self.queued_attack.take() {
                    self.resume_walking();
                    out.key = AT_RESTART;
                    self.start_attack(req, target, false, ctx, out);
                    for (key, event) in std::mem::take(&mut self.deferred) {
                        out.push_at(key, event);
                    }
                }
            }
            Due::MoveRestart { seq } => {
                let Some(h) = self.move_hold else {
                    return;
                };
                if h.seq != seq {
                    return;
                }
                if ctx.frame - h.last_input > MoveHold::STALE_FRAMES {
                    self.move_hold = None;
                    return;
                }
                // A newer press or repeat restarted the timer.
                if ctx.frame < h.last_start + CLICK_RESTART_FRAMES {
                    return;
                }
                self.due
                    .push((ctx.frame + CLICK_RESTART_FRAMES, Due::MoveRestart { seq }));
                // Only while the walk runs: a roll or sprint holds the restart back, and the
                // roll's end issues one.
                if matches!(self.state, MoveState::Walking(Walk::Target { .. }))
                    && !self.in_click_sprint_end()
                {
                    out.key = AT_RESTART;
                    out.push(MoveEvent::ActionStart {
                        kind: ActionKind::Move,
                        skill: cfg.skills.move_skill,
                        flags: h.flags & !ACTION_INPUT_FLAG,
                        action: Self::alloc_action(ctx),
                        cell: self.sim.grid(),
                        target: h.goal,
                        entity: None,
                        unk: 0xff,
                    });
                    if let Some(m) = &mut self.move_hold {
                        m.last_start = ctx.frame;
                    }
                }
            }
            Due::StartDeferredRoll => {
                if let Some((req, target)) = self.deferred_roll.take() {
                    if self.is_rolling() {
                        self.queued_roll = Some((req, target));
                    } else {
                        out.key = if self.roll_late { AT_RESTART } else { AT_ROLL_END_MOVE };
                        self.start_roll(req, target, false, ctx, out);
                    }
                }
            }
            Due::UnscaledAck => *unscaled = true,
            Due::DirectionOver => {
                self.wasd = (0, 0);
                *unscaled = true;
            }
            Due::WallStunOff { index } => {
                out.key = AT_BUFFS;
                out.push(MoveEvent::BuffRemoved { index });
                out.push(MoveEvent::Stats(vec![(MoveStat::AllHitsStunYou, 0)]));
            }
            Due::RollStunStats => {
                out.push_at(AT_ROLL_STUN, MoveEvent::Stats(stun_stats(ROLL_STUN_THRESHOLD)));
            }
            Due::Stance(combat) => {
                let entered = std::mem::replace(&mut self.stance_entered, true);
                // Before entry a request waits: 2026-10-14 (four sessions, `0x200` value 1 and
                // mode 0 right after mode 1 at f1) answers it one frame after the entry stance.
                let request = if entered {
                    self.stance_request.take()
                } else {
                    None
                };
                if self.combat_stance != combat || request.is_some() {
                    self.combat_stance = combat;
                    let value = if combat {
                        STANCE_LERP_COMBAT
                    } else {
                        STANCE_LERP_PEACEFUL
                    };
                    out.key = Drain::Start;
                    out.push(MoveEvent::OrientationLerp { value });
                    out.push(MoveEvent::StanceChanged { combat, request });
                }
                if !entered && self.stance_request.is_some() {
                    self.due.push((ctx.frame + 1, Due::Stance(self.aim_combat)));
                }
            }
        }
    }

    /// Walks in the held direction, or stops.
    fn resume_walking(&mut self) {
        if self.wasd != (0, 0) {
            self.state = MoveState::Walking(Walk::Direction(self.wasd));
        } else {
            self.state = MoveState::Idle;
        }
    }

    // ---- replies of the frame

    fn move_ack(&mut self, unscaled_due: bool, _ctx: &StepCtx, out: &mut Out) {
        // The client's own grid cell: it snaps to any other cell in a non-zero `0x64`
        // (lockstep), which is the hitch at every WASD start and turn and every frame of a
        // sprint stream when the cell of the position is sent instead.
        let cell = self.sim.grid();
        let wasd = self.wasd;
        let scaled = scale_to(wasd, SPRINT_DIR_LENGTH);
        match self.sprint() {
            Some(run) if run.streaming => out.push(MoveEvent::MoveAck { dir: scaled, cell }),
            _ if self.ack_wanted || unscaled_due => {
                out.push(MoveEvent::MoveAck { dir: wasd, cell })
            }
            _ => {}
        }
        self.ack_wanted = false;
    }

    /// The sprint key went up: sprint-off buffs and stats after this frame's `MoveAck`, then
    /// back to walking. The stream's last message was scaled and the next frame sends the
    /// unscaled direction.
    ///
    /// `first`: no `0x64` goes out in this frame, so the sprint-off has `(player, 9, 4)`. Its seq is
    /// the phase at which the client's `SprintPart_End` runs (world `+0x67e`; hub
    /// `docs/re/movement-sprint.md`, "Keys of the sprint-off"): a later seq runs after the end, and
    /// the `SprintEnd` clip takes the sprint's rate (game test crazy-keys, 8 of 8).
    fn end_sprint(&mut self, first: bool, ctx: &mut StepCtx, out: &mut Out) {
        let frame = ctx.frame;
        let Some(run) = self.sprint().copied() else {
            return;
        };
        self.release_pending = false;
        let off_key = if first { AT_SPRINT_END_MOVE } else { AT_SPRINT_END };
        out.key = off_key;
        let sprint_move = std::mem::take(&mut self.sprint_move);
        if sprint_move && matches!(self.state, MoveState::Sprinting(_)) {
            // The click's Move ran the sprint and the key went up (2026-10-20_21-51-49 f8952,
            // 2026-11-02 f7910): no `StartAction`, `(0, 0)` in the next frame, and the client
            // plays `SprintEnd`: presses and repeats are only acked until it is over (a press
            // 21 frames after the batch, f8973) and no restart comes.
            let dir = self.wasd;
            self.wasd = (0, 0);
            self.due.push((frame + 1, Due::UnscaledAck));
            out.key = off_key;
            out.push(MoveEvent::BuffRemoved {
                index: run.buff_base + 1,
            });
            out.push(MoveEvent::Stats(self.sprint_off_stats()));
            out.push(MoveEvent::BuffRemoved {
                index: run.buff_base + 2,
            });
            out.push(MoveEvent::Stats(vec![(MoveStat::AllHitsStunYou, 0)]));
            self.state = MoveState::Idle;
            self.sprint_end = Some(SprintEnd {
                frame,
                dir,
                cursor: true,
                coasting: true,
                click: true,
            });
            return;
        }
        if let Some(h) = self.live_hold(ctx.frame)
            && matches!(self.state, MoveState::Sprinting(_))
        {
            // A held click: the walk goes on. Ended at t0 + 35 by an earlier release, with a
            // `StartAction` before the sprint-off buffs (2026-10-20 f3643); a sprint the click
            // already steers keeps its Move (2026-10-20_21-51-49 f8952, 2026-11-02 f7910).
            let goal = h.settled_goal(ctx.frame, 1);
            self.state = MoveState::Walking(Walk::Target { goal });
            // The direction is over: the next frame's `MoveAck` is `(0, 0)` (3 of 3, with or
            // without the client's own zero vector) and later cursor vectors are not answered.
            self.wasd = (0, 0);
            self.due.push((frame + 1, Due::UnscaledAck));
            out.key = AT_SPRINT_END_LATE;
            out.push_at(AT_SPRINT_END_MOVE, MoveEvent::ActionStart {
                kind: ActionKind::Move,
                skill: ctx.config.skills.move_skill,
                flags: h.flags & !ACTION_INPUT_FLAG,
                action: Self::alloc_action(ctx),
                cell: self.sim.grid(),
                target: goal,
                entity: None,
                unk: 0xff,
            });
            // The client plays `SprintEnd` all the same: the next repeat is only acked
            // (f3645, then a roll).
            self.sprint_end = Some(SprintEnd {
                frame,
                dir: (0, 0),
                cursor: true,
                coasting: false,
                click: true,
            });
        }
        out.push(MoveEvent::BuffRemoved {
            index: run.buff_base + 1,
        });
        out.push(MoveEvent::Stats(self.sprint_off_stats()));
        out.push(MoveEvent::BuffRemoved {
            index: run.buff_base + 2,
        });
        out.push(MoveEvent::Stats(vec![(MoveStat::AllHitsStunYou, 0)]));
        // The stream's last `0x64` carried the scaled vector: the next frame sends the unscaled
        // one (official sprints with a stream, 6 of 6: `0x64` unscaled at r+1). Without a
        // stream the client's direction (Actor +0x400) was never scaled, so that `0x64` would
        // change nothing but could snap: in lockstep the Actor handler (`0x1421497c0`) places
        // the entity at any non-zero `0x64` whose cell differs from its grid (+0x444)
        // (`0x141ddb6f0`, which stops the `SprintEnd` root move via `0x141de5f50`). The
        // official server's cell equals the client's; ours can be a cell off after the roll,
        // so it sends none (2026-11-02: one cell off, the client stood instead of coasting).
        if self.wasd != (0, 0) && run.streaming {
            self.due.push((frame + 1, Due::UnscaledAck));
        }
        self.finish_sprint_by_release(ctx);
    }

    /// The speed goes back to the character's own; the official list is shorter when the speed
    /// has other sources (2026-11-02 f11239: `(236, -400), (22325, 0), (22342, 0)`).
    fn sprint_off_stats(&mut self) -> Vec<(MoveStat, i32)> {
        self.sprint_percent = 0;
        let speed = self.speed_stats;
        let mut stats = vec![(MoveStat::MovementVelocityPermyriad, speed.permyriad(0))];
        if speed.is_plain() {
            stats.insert(0, (MoveStat::BaseMoveSpeedFromSprint, 0));
        }
        stats.push((MoveStat::SprintBlendTimeOverrideMs, 0));
        stats.push((MoveStat::IsSprinting, 0));
        if speed.is_plain() {
            stats.push((MoveStat::VirtualSkillIgnoreNamelockingDynamically, 0));
        }
        stats
    }

    /// The sprint ran into a wall: the client's Move action ran out and it ended the sprint
    /// itself (`0x141b76da0`, no action left -> `SprintEndBonk`, whatever the keys do). Official
    /// (3 of 3): the sprint-off in the batch where the server's own cell stops advancing, the
    /// unscaled `0x64` one frame later when a stream ran, `all_hits_stun_you` off 7 to 8 frames
    /// later; no restart while the key stays down.
    ///
    /// `SprintEndBonk` plays the clip of `SprintEnd` (0.75 s, `base_sprint.atl`) at the rate of
    /// the speed in effect when the check fired, the sprint's. The client does not move during
    /// it (official f1243, f2873; in game 2026-11-02): no coast.
    fn end_sprint_at_wall(&mut self, ctx: &mut StepCtx, out: &mut Out) {
        let Some(run) = self.sprint().copied() else {
            return;
        };
        self.release_pending = false;
        let sprint_move = std::mem::take(&mut self.sprint_move);
        out.push(MoveEvent::BuffRemoved {
            index: run.buff_base + 1,
        });
        out.push(MoveEvent::Stats(self.sprint_off_stats()));
        self.due.push((
            ctx.frame + WALL_STUN_OFF_FRAMES,
            Due::WallStunOff {
                index: run.buff_base + 2,
            },
        ));
        // The `0x64` of this very frame is the unscaled one (2026-11-03 f3064: `(player, 0, 4)`,
        // then `(player, 9, 0)` with the input's vector).
        self.ack_wanted |= self.wasd != (0, 0) && run.streaming;
        self.state = MoveState::Idle;
        self.sprint_end = None;
        self.bonk = true;
        if sprint_move {
            // The click's Move ran the sprint: as at a release, the direction is over in the next
            // frame and presses are only acked while the client plays `SprintEndBonk` (seg1 f3300).
            self.due.push((ctx.frame + 1, Due::DirectionOver));
            self.sprint_end = Some(SprintEnd {
                frame: ctx.frame,
                dir: (0, 0),
                cursor: true,
                coasting: false,
                click: true,
            });
        }
    }

    /// The sprint ended by the key release: walks on, or coasts.
    fn finish_sprint_by_release(&mut self, ctx: &StepCtx) {
        if !matches!(self.state, MoveState::Sprinting(_)) {
            self.finish_sprint();
            return;
        }
        if self.wasd != (0, 0) && self.cursor_mode {
            // Click-to-move mode has no movement keys: the vector is the cursor direction
            // whatever its angle (an exactly diagonal one looks like a key's) and the client's
            // unscaled-ack restart finds no key down. It coasts, without waiting for the
            // client's zero vector (4 to 5 frames after the batch with an exactly diagonal
            // cursor, in game 2026-10-21); that and the cursor vectors still in flight are
            // ignored for the release window.
            let dir = self.wasd;
            self.state = MoveState::Idle;
            self.sprint_end = Some(SprintEnd {
                frame: ctx.frame,
                dir,
                cursor: true,
                coasting: true,
                click: false,
            });
        } else if self.wasd != (0, 0) || self.facing != (0, 0) {
            // The client plays `SprintEnd` and coasts whether or not a key is held; the
            // unscaled `MoveAck` of the next frame restarts nothing (`Actor_StartWasdWalk`
            // refuses during the animation). A key still down at its end walks on.
            let dir = if self.wasd != (0, 0) {
                self.wasd
            } else {
                self.facing
            };
            self.state = MoveState::Idle;
            self.sprint_end = Some(SprintEnd {
                frame: ctx.frame,
                dir,
                cursor: self.is_cursor_vector(dir),
                coasting: false,
                click: false,
            });
        } else {
            self.finish_sprint();
        }
    }

    /// Whether the `0x63` vector `v` is the cursor direction (Space held, no key) and not a
    /// key: a free angle, or a key's vector that the cursor offset of the roll key points along
    /// within `cursor_match_degrees` (an exactly diagonal cursor makes the same `(127, 127)` as
    /// W, in WASD mode and with the same trailer bytes; the cursor offset tells them apart).
    fn is_cursor_vector(&self, v: Cell) -> bool {
        if !is_key_vector(v) {
            return true;
        }
        let (a, b) = (v, self.roll_cursor);
        let (la, lb) = (
            ((a.0 as f64).powi(2) + (a.1 as f64).powi(2)).sqrt(),
            ((b.0 as f64).powi(2) + (b.1 as f64).powi(2)).sqrt(),
        );
        if la == 0.0 || lb == 0.0 {
            return false;
        }
        let cos = (a.0 as f64 * b.0 as f64 + a.1 as f64 * b.1 as f64) / (la * lb);
        cos >= CURSOR_MATCH_DEGREES.to_radians().cos()
    }

    /// Leaves the sprint: a sprinting entity walks on; a rolling one finishes its roll.
    fn finish_sprint(&mut self) {
        self.sprint_move = false;
        match &mut self.state {
            MoveState::Rolling(r) => r.sprint = None,
            MoveState::Sprinting(_) => self.resume_walking(),
            _ => {}
        }
    }
}

/// A vector the keyboard can make (one axis, or both with the same magnitude), as opposed to
/// the free angle of the cursor direction the client sends with Space held and no key.
fn is_key_vector(v: Cell) -> bool {
    v.0 == 0 || v.1 == 0 || v.0.abs() == v.1.abs()
}

/// `v` scaled to length `len` and truncated toward zero (`(0, 0)` stays).
fn scale_to(v: Cell, len: i32) -> Cell {
    let l = ((v.0 as f64).powi(2) + (v.1 as f64).powi(2)).sqrt();
    if l == 0.0 {
        return (0, 0);
    }
    let k = len as f64 / l;
    ((v.0 as f64 * k) as i32, (v.1 as f64 * k) as i32)
}

fn stun_stats(value: i32) -> Vec<(MoveStat, i32)> {
    vec![
        (MoveStat::StunThresholdWhilePerformingAction, value),
        (MoveStat::VirtualHeavyStunThreshold, value),
        (MoveStat::VirtualLightStunThreshold, value),
    ]
}

fn roll_end_stats(out: &mut Out) {
    let mut stats = stun_stats(0);
    stats.push((MoveStat::IsDodgeRolling, 0));
    out.push(MoveEvent::Stats(stats));
}

impl Mover for Locomotion {
    fn set_speed_stats(&mut self, stats: SpeedStats) {
        self.pending_speed_stats = Some(stats);
    }

    fn input(&mut self, input: MoveInput) {
        Locomotion::input(self, input);
    }

    fn teleport(&mut self, cell: Cell) {
        Locomotion::teleport(self, cell);
    }

    fn tick(&mut self, ctx: &mut StepCtx) -> Vec<Keyed> {
        Locomotion::tick(self, ctx)
    }

    fn tick_paused(&mut self, ctx: &mut StepCtx) -> Vec<Keyed> {
        Locomotion::tick_paused(self, ctx)
    }

    fn cell(&self) -> Cell {
        self.sim.grid()
    }

    fn position(&self) -> (f32, f32) {
        self.sim.point()
    }

    fn grid(&self) -> Cell {
        self.sim.grid()
    }

    fn set_stance(&mut self, combat: bool) {
        Locomotion::set_stance(self, combat);
    }

    fn set_weapon_set(&mut self, set: u8) {
        Locomotion::set_weapon_set(self, set);
    }

    fn in_combat_stance(&self) -> bool {
        self.combat_stance
    }

    fn debug_fields(&self) -> Vec<(&'static str, String)> {
        let state = match self.state {
            MoveState::Idle => "Idle",
            MoveState::Walking(Walk::Direction(_)) => "Walking",
            MoveState::Walking(Walk::Target { .. }) => "Click walk",
            MoveState::Rolling(_) => "Rolling",
            MoveState::Sprinting(_) => "Sprinting",
        };
        vec![
            ("state", state.into()),
            ("wasd", format!("{:?}", self.wasd)),
            ("sprint_key", self.held.to_string()),
            ("coasting", self.sim.is_coasting().to_string()),
        ]
    }
}
