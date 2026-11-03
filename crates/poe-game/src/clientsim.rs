//! Exact port of the client's movement code for one entity: Positioned (line, step check, facing),
//! Pathfinding (follow path, waypoints, sprint turn, arrival) and the Actor's movement part (Move
//! slot, WASD axes, plan timer, roll, sprint, coast). The spec is hub `docs/re/movement-*.md`;
//! function comments give the client address (Hotfix 1). All math is `f32`, like the client's.
//!
//! The client's delegates are direct calls here. A frame runs in the client's order: each component
//! ticks in list order (0 Positioned, 2 Pathfinding, 5 Animated, 9 Actor), and the messages keyed to
//! a component (header order key, component, sequence: hub `docs/protocol/instance.md`, Frame
//! ordering) are drained just before its tick. So a `0x1c8` keyed `(0, 0, 0)` runs before the
//! Positioned tick and a `0x64` keyed to the Actor sees the grid after it.

use std::sync::Arc;

use crate::clientpath::{self, Params};
use crate::motion_data::{Curve, MotionCurves};
use crate::movement::{Cell, Walkable};

/// An attack's speed factor, measured (2026-10-20 capture, 4 attacks while walking): the factor it
/// falls to, its ramp per second, the frame after the start at which it drops at once (the
/// animation's contact event), and the frame from which it ramps back (the lock).
const ACTION_SLOW_FLOOR: f32 = 0.3;
const ACTION_SLOW_RAMP: f32 = 1.5;
const ACTION_SLOW_SNAP_FRAMES: u64 = 7;
const ACTION_SLOW_LOCK_FRAMES: u64 = 12;

const CELL: f32 = 10.869565;
const TO_CELL: f32 = 0.092;
const PI: f32 = std::f32::consts::PI;
const TAU: f32 = 6.283_185_5;
const HALF_PI: f32 = 1.570_796_4;
/// `Metadata/Characters/Character.ot` `dodge_roll_sprint_turn_rate`, degrees per second.
const SPRINT_TURN_RATE: f32 = 150.0;
/// `DodgeRoll_StartSprint` `0x141aa92e0`: the optional line length of a roll that sprints.
const ROLL_SPRINT_LINE: i32 = 300;
/// `GP_sprinting_end_base_01`: the move event's end time, seconds of the clip.
const SPRINT_END_MOVE_TIME: f32 = 0.75;
/// `sprinting_end_*` entry speed (`0x141e56ae0`).
const SPRINT_END_BASE_SPEED: f32 = 600.0;
/// `0x141e42650`: the plan timer of an entity with a Player component.
const PLAN_TIMER: f32 = 0.05;
/// Recursion the client's own delegates cannot reach on real paths (waypoints on the start cell).
const MAX_DEPTH: u32 = 8;

fn from_world(w: f32) -> i32 {
    (w * TO_CELL).floor() as i32
}

fn world(c: i32) -> f32 {
    (c as f32 + 0.5) * CELL
}

/// `0x1405226a0`.
fn round(x: f32) -> i32 {
    (x + 0.5).floor() as i32
}

fn wrap(a: f32) -> f32 {
    let a = a % TAU;
    if a < 0.0 { a + TAU } else { a }
}

/// The facing convention of `atan2 + pi/2`.
fn angle(dx: f32, dy: f32) -> f32 {
    wrap(dy.atan2(dx) + HALF_PI)
}

/// `0x142196700`.
fn rot(x: f32, y: f32, a: f32) -> (f32, f32) {
    let (s, c) = a.sin_cos();
    (x * c - y * s, x * s + y * c)
}

/// The movement curve run, `Positioned + 0x2a8` (`L + 0xa0`), positions branch only.
#[derive(Debug, Clone, Default)]
struct CurveRun {
    curve: Option<Curve>,
    /// `[0]`, `[1]`.
    range: f32,
    /// `[3]`, `[4]`.
    c0: f32,
    c_end: f32,
    /// `[7]`, `[8]`.
    left: f32,
    t: f32,
}

impl CurveRun {
    /// `0x141ecc540`.
    fn load(&mut self, curve: Curve, s: f32, e: f32) {
        self.curve = Some(curve);
        self.set_time(s, e);
        let at_t = self.value(self.t);
        self.c0 = at_t;
        self.c_end = self.value(e);
    }

    /// `0x141ecc710`.
    fn set_time(&mut self, t: f32, e: f32) {
        self.t = t;
        self.left = e - t;
        self.range = self.value(e) - self.value(t);
    }

    fn value(&self, t: f32) -> f32 {
        self.curve.as_ref().map_or(0.0, |c| c.value(t))
    }

    /// `0x141ecc780`.
    fn advance(&mut self, dt: f32, r: f32) -> f32 {
        let old = self.value(self.t);
        self.t += self.left * r * dt;
        (self.value(self.t) - old) / self.range
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

/// The path line, `Positioned + 0x208`. After the swap "x" = major, "y" = minor.
#[derive(Debug, Clone, Default)]
struct Line {
    p: (f32, f32),
    q: (i32, i32),
    flag: bool,
    swap: bool,
    sign: i32,
    slope: f32,
    end: (i32, i32),
    cur: (i32, i32),
    minor_acc: f32,
    distance_mode: bool,
    len: f32,
    t_total: f32,
    inv_t: f32,
    opt_rate: f32,
    elapsed: f32,
    distance: f32,
    thr: f32,
    acc: f32,
    pending: f32,
    step_time: f32,
    curve_rate: f32,
    curve: CurveRun,
}

impl Line {
    fn unswap(&self, v: (i32, i32)) -> Cell {
        if self.swap { (v.1, v.0) } else { v }
    }

    /// `Line_Point` `0x141ec83d0`, unswapped.
    fn point(&self) -> (f32, f32) {
        let f = if self.distance_mode {
            self.distance / self.len
        } else {
            self.elapsed / self.t_total
        };
        let f = f.clamp(0.0, 1.0);
        let major = self.p.0 + (self.end.0 as f32 - self.p.0) * f;
        let minor = self.p.1 + (self.end.1 as f32 - self.p.1) * f;
        if self.swap { (minor, major) } else { (major, minor) }
    }

    /// `Line_Setup` `0x141ec86a0`, 2D.
    fn setup(&mut self, s: Cell, e: Cell, speed: f32) {
        let sf = (s.0 as f32, s.1 as f32);
        let mut running = self.len != 0.0 && self.unswap(self.cur) == s;
        let mut p = sf;
        if running {
            let old = self.point();
            let d2 = (sf.0 - old.0).powi(2) + (sf.1 - old.1).powi(2);
            if (old.0 == e.0 as f32 && old.1 == e.1 as f32) || d2 >= 4.0 {
                running = false;
            } else {
                p = old;
            }
        }
        let mut dir = (e.0 as f32 - p.0, e.1 as f32 - p.1);
        let mut q = s;
        self.flag = false;
        if running {
            let t = (dir.0 * (sf.0 - p.0) + dir.1 * (sf.1 - p.1)) / (dir.0 * dir.0 + dir.1 * dir.1);
            q = (round(dir.0 * t + p.0), round(dir.1 * t + p.1));
            self.flag = q != s;
        }
        let d = (e.0 - q.0, e.1 - q.1);
        self.swap = d.0.abs() < d.1.abs()
            || (running && d.0.abs() == d.1.abs() && dir.0.abs() < dir.1.abs());
        let mut e = e;
        let mut s_major = s;
        if self.swap {
            q = (q.1, q.0);
            p = (p.1, p.0);
            dir = (dir.1, dir.0);
            e = (e.1, e.0);
            s_major = (s.1, s.0);
        }
        self.distance_mode = self.curve.curve.is_some();
        self.cur = s_major;
        self.end = e;
        self.q = q;
        self.p = p;
        let len = (dir.0 * dir.0 + dir.1 * dir.1).sqrt();
        self.len = len;
        self.t_total = len / speed;
        self.inv_t = 1.0 / self.t_total;
        self.opt_rate = 0.0;
        let major = dir.0.abs();
        let inv = 1.0 / major;
        self.curve_rate = 1.0;
        self.step_time = self.t_total * inv;
        self.thr = if self.distance_mode { len } else { self.t_total } * inv;
        self.slope = dir.1 * inv;
        let k = (e.0 - q.0).abs();
        self.sign = if q.0 < e.0 { 1 } else { -1 };
        self.elapsed = 0.0;
        self.distance = 0.0;
        let k0 = k as f32 - major;
        self.acc = self.thr * k0;
        self.minor_acc =
            (p.1 - self.slope * k0) + if self.slope > 0.0 { 0.49 } else { 0.5 };
    }

    /// `Line_Advance` `0x141ec82e0`.
    fn advance(&mut self, dt: f32) {
        let mut x = dt + self.pending;
        self.pending = x;
        self.elapsed += x;
        if self.distance_mode {
            let rate = if self.opt_rate > 0.0 { self.opt_rate } else { self.inv_t };
            let f = self.curve.advance(x, rate);
            self.curve_rate = if f <= 0.0 { 1.0 } else { rate * self.pending / f };
            x = if self.opt_rate > 0.0 {
                self.inv_t / self.opt_rate * f * self.len
            } else {
                f * self.len
            };
        }
        self.pending = 0.0;
        self.acc += x;
        self.distance += x;
    }

    /// `Line_Step` `0x141ec8cb0`: the cell and whether the line ended.
    fn step(&mut self) -> (Cell, bool) {
        if !self.flag {
            if self.thr <= self.acc {
                self.cur.0 += self.sign;
                self.acc -= self.thr;
                self.minor_acc += self.slope;
                self.cur.1 = self.minor_acc.floor() as i32;
            }
        } else {
            self.cur.0 += (self.q.0 - self.cur.0).signum();
            self.cur.1 += (self.q.1 - self.cur.1).signum();
            if self.cur == self.q {
                self.flag = false;
                if self.cur.0 == self.end.0 {
                    self.acc -= if self.distance_mode { self.len } else { self.t_total };
                }
            }
        }
        let done = self.cur.0 == self.end.0;
        let cell = self.unswap(self.cur);
        if done {
            self.finish();
        }
        (cell, done)
    }

    /// `Line_End` `0x141ec8f00`.
    fn finish(&mut self) {
        if self.thr != 0.0 {
            self.pending = if self.distance_mode {
                self.curve_rate * self.step_time * (self.acc / self.thr)
            } else {
                self.acc
            };
            if self.pending < 0.0 {
                self.pending = 0.0;
            }
            self.thr = 0.0;
            self.len = 0.0;
        }
    }
}

/// The Move slot, `Actor + 0x240`.
#[derive(Debug, Clone, Copy)]
struct MoveAction {
    /// `+0x150`.
    target: Cell,
    /// `+0x122` != 0: numbered by the server (a click walk); never re-aimed.
    from_server: bool,
}

/// The roll action's movement part (`docs/re/movement-roll.md`).
#[derive(Debug, Clone)]
struct Roll {
    /// `ac + 0x1a4`.
    elapsed: f32,
    /// `ac + 0x1bc - ac + 0x1b8`.
    length: f32,
    /// `ac + 0x178 * ac + 0x1b0`.
    rate: f32,
    /// `+0x40`.
    heading: f32,
    /// `+0x3f`.
    backward: bool,
    /// `+0x4c`.
    total: i32,
    /// `+0x3e`.
    waiting: bool,
    /// The action's `+0x24d`: the movement vector took over, `0x1af` no longer aims it.
    axes_aimed: bool,
    /// `+0x44`, `+0x48`.
    opt_len: Option<i32>,
    /// A sprint child was started (`act + 0x448`).
    sprints: bool,
    /// The animation event `0xb91f` has fired.
    event_done: bool,
    /// `act + 0x240`: inside the timeline's `StartTargetUpdateEventType` window.
    target_update: bool,
    /// `act + 0x250` with `act + 0x258`: the wanted target relative to the grid.
    pending_target: Option<Cell>,
    /// `act + 0x150`.
    target: Cell,
}

/// Which animation refuses walks (`Actor + 0x380` in `0x36a`, `0x36b`).
#[derive(Debug, Clone, Copy)]
struct SprintEndAnim {
    /// Seconds of real time left.
    left: f32,
    /// The clip's rate.
    rate: f32,
    /// The move event starts in the next animation tick.
    coast_due: bool,
}

/// One entity's client movement state.
#[derive(Debug, Clone)]
pub struct ClientSim {
    // Positioned
    grid: Cell,
    facing: f32,
    line: Line,
    /// `+0x1e5` bit 7.
    stop_pending: bool,
    /// `+0x1e3` orientation mode.
    orient: u8,
    /// Search level (`FUN_141dc7d00`).
    level: i32,
    // Pathfinding
    following: bool,
    /// `pf + 0x540`.
    goal: Option<Cell>,
    plan_goal: Cell,
    since_plan: f32,
    dt: f32,
    /// Goal-first, start last (`pf + 0xd4`).
    path: Vec<Cell>,
    index: i32,
    /// `pf + 0x560` bit 2: the pathfinder itself is moving the line.
    in_step: bool,
    base_speed: f32,
    permyriad: i32,
    /// `pf + 0x55c`.
    speed_factor: f32,
    /// The frame a running attack's speed factor counts from.
    action_slow: Option<u64>,
    /// Frames ticked (ends with the Actor tick).
    frames: u64,
    depth: u32,
    // Actor
    axes: (i32, i32),
    axes_fresh: bool,
    mv: Option<MoveAction>,
    /// `+0x2a0` bit 7.
    moving: bool,
    plan_timer: f32,
    /// `+0x2a1` bit 2.
    in_plan: bool,
    roll: Option<Roll>,
    /// `A + 0x248`: a server Move that waits for the roll to end, `(start, target)`.
    queued_move: Option<(Cell, Cell)>,
    /// The sprint part runs its end check.
    sprint: bool,
    sprint_end: Option<SprintEndAnim>,
    bonked: bool,
    // Stats and keys
    is_sprinting: bool,
    must_sprint: bool,
    /// `+0x2a2` bit 5.
    sprint_key: bool,
    /// Player `+0x20a`, from `0x2e8`.
    pub continues_with_move_input: bool,
    /// The clearance of the sprint (`max(+0x1f8, +0x1f9)`).
    pub sprint_level: i32,
    curves: Option<Arc<MotionCurves>>,
    offset_scale: f32,
    /// Offline replay only: the client's logged plans `(start, goal, path)`, which contain entities
    /// the replay's terrain does not have (`examples/sim_replay.rs`).
    pub plan_hints: Vec<(Cell, Cell, Vec<Cell>)>,
}

impl ClientSim {
    pub fn new(cell: Cell, walk_speed: f32, curves: Option<Arc<MotionCurves>>, offset_scale: f32) -> Self {
        Self {
            grid: cell,
            facing: 0.0,
            line: Line::default(),
            stop_pending: false,
            orient: 1,
            level: 0,
            following: false,
            goal: None,
            plan_goal: cell,
            since_plan: 0.0,
            dt: 0.033,
            path: Vec::new(),
            index: 0,
            in_step: false,
            base_speed: walk_speed,
            permyriad: 0,
            speed_factor: 1.0,
            action_slow: None,
            frames: 0,
            depth: 0,
            axes: (0, 0),
            axes_fresh: false,
            mv: None,
            moving: false,
            plan_timer: 0.0,
            in_plan: false,
            roll: None,
            queued_move: None,
            sprint: false,
            sprint_end: None,
            bonked: false,
            is_sprinting: false,
            must_sprint: false,
            sprint_key: false,
            continues_with_move_input: false,
            sprint_level: 4,
            curves,
            offset_scale,
            plan_hints: Vec::new(),
        }
    }

    pub fn grid(&self) -> Cell {
        self.grid
    }

    pub fn facing(&self) -> f32 {
        self.facing
    }

    pub fn axes(&self) -> (i32, i32) {
        self.axes
    }

    /// The line's start point, length and accumulator as the client logs them.
    pub fn line_state(&self) -> ((f32, f32), f32, f32) {
        let l = &self.line;
        (if l.swap { (l.p.1, l.p.0) } else { l.p }, l.len, l.acc)
    }

    /// The line's end while one runs.
    pub fn line_end(&self) -> Option<Cell> {
        (self.line.thr != 0.0).then(|| self.line.unswap(self.line.end))
    }

    /// A Move action holds the Move slot.
    pub fn has_move(&self) -> bool {
        self.mv.is_some()
    }

    pub fn is_rolling(&self) -> bool {
        self.roll.is_some()
    }

    pub fn is_coasting(&self) -> bool {
        self.sprint_end.is_some()
    }

    pub fn is_moving(&self) -> bool {
        self.line.thr != 0.0
    }

    /// The sprint ended at a wall since the last call.
    pub fn take_bonked(&mut self) -> bool {
        std::mem::take(&mut self.bonked)
    }

    /// The exact position in cell-centred coordinates (the line point, else the cell).
    pub fn point(&self) -> (f32, f32) {
        if self.line.thr != 0.0 {
            self.line.point()
        } else {
            (self.grid.0 as f32, self.grid.1 as f32)
        }
    }

    /// The path being followed, goal first.
    pub fn path(&self) -> &[Cell] {
        &self.path
    }

    /// `pf + 0x55c`, the Actor's action speed factor.
    pub fn speed_factor(&self) -> f32 {
        self.speed_factor
    }

    /// The roll's clip reaches its `sprint_start` event in the Animated tick of this frame.
    pub fn roll_event_due(&self, dt: f32) -> bool {
        match (&self.roll, &self.curves) {
            (Some(r), Some(c)) => {
                !r.event_done && r.elapsed + dt * r.rate >= c.roll_events.sprint_start
            }
            _ => false,
        }
    }

    /// The roll's clip ends in the Animated tick of this frame.
    pub fn roll_end_due(&self, dt: f32) -> bool {
        self.roll.as_ref().is_some_and(|r| r.elapsed + dt * r.rate >= r.length)
    }

    /// `Pathfinding_GetSpeed` `0x141dccfa0`.
    pub fn speed(&self) -> f32 {
        let f = self.permyriad.max(-10000) as f32 / 10000.0 + 1.0;
        self.base_speed * f * self.speed_factor
    }

    /// Places the entity: server teleports and spawn.
    pub fn place(&mut self, cell: Cell) {
        self.snap(cell);
    }

    // ---- frame start: Positioned tick, Pathfinding tick, animation tick ----

    /// Component 0: the Positioned tick `0x141ddc950`.
    pub fn positioned_tick(&mut self, dt: f32, t: &dyn Walkable) {
        self.action_slow_tick(dt);
        self.run_line(dt, t);
    }

    /// The action speed factor of a running attack (Actor `+0x444`, target `+0x448`, ramp
    /// `0x141e441ab`, `0x14197ea40`, `0x14197eb10`). The client takes the target, ramp, contact
    /// event and lock from the skill's stats and the animation; those are not modelled, so this
    /// runs the values measured on the client's steps (hub `docs/protocol/movement.md`).
    fn action_slow_tick(&mut self, dt: f32) {
        let Some(start) = self.action_slow else {
            return;
        };
        let n = self.frames - start;
        if n == 0 {
            return;
        }
        let rate = ACTION_SLOW_RAMP * dt;
        if n <= ACTION_SLOW_LOCK_FRAMES {
            self.speed_factor = if n >= ACTION_SLOW_SNAP_FRAMES {
                ACTION_SLOW_FLOOR
            } else {
                (self.speed_factor - rate).max(ACTION_SLOW_FLOOR)
            };
        } else {
            self.speed_factor = (self.speed_factor + rate).min(1.0);
            if self.speed_factor >= 1.0 {
                self.action_slow = None;
            }
        }
    }

    /// `Positioned_RunLine` `0x141ddc9d0`.
    fn run_line(&mut self, dt: f32, t: &dyn Walkable) {
        if self.line.thr == 0.0 {
            return;
        }
        self.line.advance(dt);
        while self.line.thr != 0.0 && (self.line.thr <= self.line.acc || (self.line.flag && self.line.acc > 0.0)) {
            let (cell, done) = self.line.step();
            if self.step_blocked(cell, t) {
                self.stop_move();
                if !self.stop_pending {
                    self.end_path(false, t);
                }
                // Measured (official `0x1d3` keyed to component 0, no plan on the `0x64` that follows,
                // no coast): a blocked step ends the sprint in the Positioned tick with the Bonk clip.
                if self.sprint && self.roll.is_none() {
                    self.bonked = true;
                    self.sprint_part_end(true, t);
                }
            } else {
                self.grid = cell;
                if !done {
                    continue;
                }
                if !self.stop_pending {
                    self.on_line_done(t);
                }
            }
            if self.line.thr == 0.0 {
                self.line.acc = 0.0;
                self.line.pending = 0.0;
                break;
            }
            if self.line.pending > 0.0 {
                self.line.advance(0.0);
            }
        }
        if self.stop_pending {
            self.stop_pending = false;
            self.line.finish();
            self.line.acc = 0.0;
            self.line.pending = 0.0;
        }
    }

    /// `Positioned_CheckStep` `0x141ddcfc0`, terrain part: outside, or below level 1 on the
    /// mode's layer (2 while sprinting, mode 0 otherwise); roll and curve lines use mode 0 too.
    /// Entity collision is not ported: see the open items of `docs/re/movement-positioned.md`.
    fn step_blocked(&self, c: Cell, t: &dyn Walkable) -> bool {
        if let Some((w, h)) = t.bounds()
            && (c.0 < 0 || c.1 < 0 || c.0 >= w || c.1 >= h)
        {
            return true;
        }
        let layer = 0;
        t.layer(layer, c.0, c.1) < 1
    }

    /// Component 2: `Pathfinding_Tick` `0x141dcce80`.
    pub fn pathfinding_tick(&mut self, dt: f32, t: &dyn Walkable) {
        self.dt = dt;
        self.since_plan += dt;
        if !self.following {
            return;
        }
        match self.goal {
            Some(g) if !self.is_arrived(g, t) => {
                if self.since_plan >= 2.0 && self.plan_goal != g {
                    self.plan(t);
                } else if self.since_plan >= 0.7 {
                    let d = (self.plan_goal.0 - g.0, self.plan_goal.1 - g.1);
                    if ((d.0 * d.0 + d.1 * d.1) as f32) >= 4096.0 {
                        self.plan(t);
                    }
                }
            }
            _ => self.end_path(false, t),
        }
    }

    /// Component 5: `AnimPlayer_Advance` `0x141d44fb0` for the two clips the movement reads, and
    /// the move event (`MoveEvent_Start` `0x141f5f650`).
    pub fn animated_tick(&mut self, dt: f32, t: &dyn Walkable) {
        if let Some(r) = &mut self.roll {
            let new = (r.elapsed + dt * r.rate).min(r.length);
            let ev = self.curves.as_ref().map(|c| c.roll_events);
            // `AnimationController` event type `0x1a` -> `0x141979b60`: the window opens.
            if let Some(ev) = ev
                && new >= ev.target_update.0
            {
                r.target_update = new < ev.target_update.1;
            }
            // `AnimPlayer_Advance`: the clock stands at the event's time while the event runs.
            if let Some(ev) = ev
                && !r.event_done
                && new >= ev.sprint_start
            {
                r.event_done = true;
                r.elapsed = ev.sprint_start;
                self.roll_event();
            }
            let r = self.roll.as_mut().unwrap();
            r.elapsed = new;
            // The end of the clip ends the action here, so the Actor tick of this frame can plan.
            if r.elapsed >= r.length {
                self.roll_end(t);
            }
        }
        if let Some(a) = self.sprint_end {
            if a.coast_due {
                self.sprint_end.as_mut().unwrap().coast_due = false;
                self.start_coast(a.rate, t);
            }
            // The clip's rate follows the speed: at a wall the sprint-off's stats come right after
            // the step that ended it (seg2 f800: walks are refused to f828, 30 frames at walking
            // speed, not the 20 of the sprint's).
            let rate = self.speed() * CELL / (SPRINT_END_BASE_SPEED * self.offset_scale);
            let a = self.sprint_end.as_mut().unwrap();
            a.left -= dt * rate;
            if a.left <= 0.0 {
                self.sprint_end = None;
                // The move event ends with the clip: its line stops where it is.
                self.stop_move();
                self.line.acc = 0.0;
                self.line.pending = 0.0;
            }
        }
    }

    // ---- Positioned ----

    /// `Positioned_SetLocationForcedNoCallbacks` `0x141ddb6f0`.
    fn snap(&mut self, cell: Cell) {
        self.grid = cell;
        if self.line.thr != 0.0 {
            self.stop_move();
        }
        self.line.acc = 0.0;
        self.line.pending = 0.0;
    }

    /// `Positioned_StopMove` `0x141de5f50` (the world smoother is not modelled).
    fn stop_move(&mut self) {
        self.line.finish();
        self.stop_pending = false;
    }

    /// `0x141de4c30`: the line runs through the next Positioned tick, then stops (`+0x1e5` bit 7).
    fn stop_next_tick(&mut self, t: &dyn Walkable) {
        if self.line.thr != 0.0 && !self.stop_pending {
            self.stop_pending = true;
            self.on_stopped(t);
        }
    }

    /// `0x141de4de0`: stop with callbacks.
    fn stop_with_callbacks(&mut self, t: &dyn Walkable) {
        if self.line.thr == 0.0 {
            return;
        }
        self.stop_move();
        self.on_stopped(t);
        if self.line.thr == 0.0 {
            self.line.acc = 0.0;
            self.line.pending = 0.0;
        }
    }

    /// `Positioned_MoveToWaypoint` `0x141de31a0`. `reset_curves` is request `+0x9b`.
    fn move_to_waypoint(&mut self, target: Cell, speed: f32, reset_curves: bool, t: &dyn Walkable) -> bool {
        if reset_curves {
            self.line.curve.clear();
        }
        if self.line.thr != 0.0 && !self.stop_pending {
            self.on_stopped(t);
        }
        if speed == 0.0 {
            self.stop_move();
            self.stop_pending = false;
            return false;
        }
        if target == self.grid {
            self.on_line_done(t);
            return false;
        }
        self.stop_pending = false;
        if matches!(self.orient, 1 | 3 | 4) {
            self.facing = angle((target.0 - self.grid.0) as f32, (target.1 - self.grid.1) as f32);
        } else if self.orient == 2 {
            let a = angle((target.0 - self.grid.0) as f32, (target.1 - self.grid.1) as f32);
            self.facing = wrap(a - PI);
        }
        self.line.setup(self.grid, target, speed);
        true
    }

    /// `0x141b87580`: a line runs and its current cell is the grid.
    fn on_line(&self) -> bool {
        self.line.len != 0.0 && self.line.unswap(self.line.cur) == self.grid
    }

    // ---- Pathfinding ----

    /// Listener vtable `0x1434d19c8` `+0x18` (`0x141dcce60`).
    fn on_stopped(&mut self, t: &dyn Walkable) {
        if !self.in_step {
            self.end_path(true, t);
        }
    }

    /// `Pathfinding_OnLineDone` `0x141dccdb0`.
    fn on_line_done(&mut self, t: &dyn Walkable) {
        if !self.following {
            return;
        }
        if let Some(g) = self.goal
            && self.is_arrived(g, t)
        {
            self.end_path(false, t);
            return;
        }
        self.index -= 1;
        if self.index > 0 {
            self.step_along(self.index, t);
        } else {
            self.plan(t);
        }
    }

    /// `Pathfinding_IsArrived` `0x141dcb810` for a point goal (`r0 = r1 = 0`, `n = 1`).
    fn is_arrived(&self, g: Cell, t: &dyn Walkable) -> bool {
        let d = (self.grid.0 - g.0, self.grid.1 - g.1);
        let d2 = d.0 * d.0 + d.1 * d.1;
        d2 == 0 || (d2 == 1 && t.layer(0, g.0, g.1) < 1)
    }

    /// `Pathfinding_FollowPath` `0x141dcbf30`.
    fn follow_path(&mut self, goal: Cell, t: &dyn Walkable) {
        self.goal = Some(goal);
        if self.is_arrived(goal, t) {
            if self.following {
                self.end_path(false, t);
            } else {
                self.goal = None;
                self.on_path_ended(true, t);
            }
            return;
        }
        if self.speed() <= 0.0 {
            self.end_path(false, t);
            return;
        }
        self.plan(t);
    }

    /// `Pathfinding_Plan` `0x141dcc6a0`.
    fn plan(&mut self, t: &dyn Walkable) {
        self.since_plan = 0.0;
        self.depth += 1;
        let Some(goal) = self.goal.filter(|_| self.depth <= MAX_DEPTH) else {
            self.depth -= 1;
            self.end_path(false, t);
            return;
        };
        let d = (goal.0 - self.grid.0, goal.1 - self.grid.1);
        let target = if d.0 * d.0 + d.1 * d.1 < 0x4840 {
            goal
        } else {
            let l = ((d.0 * d.0 + d.1 * d.1) as f32).sqrt();
            (
                self.grid.0 + (d.0 as f32 / l * 126.0) as i32,
                self.grid.1 + (d.1 as f32 / l * 126.0) as i32,
            )
        };
        self.path = self.search(target, t);
        if self.path.is_empty() {
            if self.following {
                self.end_path(false, t);
            } else {
                self.goal = None;
                self.on_path_ended(false, t);
            }
            self.depth -= 1;
            return;
        }
        self.index = self.path.len() as i32 - 1;
        self.plan_goal = goal;
        if !self.following {
            self.following = true;
            self.moving = true;
        }
        self.in_step = true;
        self.step_along(self.index, t);
        self.in_step = false;
        self.depth -= 1;
    }

    /// The search `0x141ed0510` (`clientpath`, an exact port checked against the client's plans).
    fn search(&self, goal: Cell, t: &dyn Walkable) -> Vec<Cell> {
        if let Some(p) = self.plan_hints.iter().find(|h| h.0 == self.grid && h.1 == goal) {
            return p.2.clone();
        }
        let moving = self.on_line().then(|| self.line.point());
        let params = Params { moving, ..Params::default() };
        clientpath::plan(t, self.grid, goal, self.level, &params).path
    }

    /// `Pathfinding_StepAlongPath` `0x141dcc110`.
    fn step_along(&mut self, i: i32, t: &dyn Walkable) {
        let w = if i >= 1 { self.path[i as usize - 1] } else { self.grid };
        let w = self.sprint_turn(w);
        let speed = self.speed();
        if self.following {
            self.move_to_waypoint(w, speed, true, t);
        }
    }

    /// `Pathfinding_SprintTurnWaypoint` `0x141dcc460`.
    fn sprint_turn(&self, w: Cell) -> Cell {
        if !self.is_sprinting {
            return w;
        }
        let dx = ((w.0 - self.grid.0) as f32 + 0.5) * CELL;
        let dy = ((w.1 - self.grid.1) as f32 + 0.5) * CELL;
        let l = (dx * dx + dy * dy).sqrt();
        let d = (dx / l, dy / l);
        let f = (self.facing.sin(), -self.facing.cos());
        let c = f.0 * d.0 + f.1 * d.1;
        if c > 0.99 {
            return w;
        }
        let mut a = SPRINT_TURN_RATE * self.dt * 0.017_453_292;
        if c > -0.99 && c.acos().abs() < a {
            return w;
        }
        if d.1 * f.0 - d.0 * f.1 < 0.0 {
            a = -a;
        }
        let (s, co) = a.sin_cos();
        let r = (f.0 * co - f.1 * s, f.0 * s + f.1 * co);
        (
            from_world(world(self.grid.0) + r.0 * 180.0 * CELL),
            from_world(world(self.grid.1) + r.1 * 180.0 * CELL),
        )
    }

    /// `Pathfinding_EndPath` `0x141dccac0`.
    fn end_path(&mut self, keep_line: bool, t: &dyn Walkable) {
        if !self.following {
            return;
        }
        if !keep_line {
            // Entity `+0x8c` bit 3 selects `0x141de4c30` over `0x141de4de0`; the player's line runs on
            // one tick (measured: the stop step).
            let in_step = std::mem::replace(&mut self.in_step, true);
            self.stop_next_tick(t);
            self.in_step = in_step;
        }
        let arrived = self.goal.is_some_and(|g| self.is_arrived(g, t));
        self.following = false;
        self.path.clear();
        self.goal = None;
        self.on_path_ended(arrived, t);
    }

    // ---- Actor ----

    /// `Actor_OnPathEnded` `0x141e4dfa0`.
    fn on_path_ended(&mut self, arrived: bool, t: &dyn Walkable) {
        self.moving = false;
        self.line.curve.clear();
        let wasd = self.mv.is_some_and(|m| !m.from_server) && self.axes != (0, 0);
        if wasd && (arrived || !self.in_plan) {
            self.reaim(t);
        } else if !arrived || self.mv.is_some() {
            self.end_move(false, t);
        }
    }

    /// `Actor_EndMoveAction` `0x141e45da0`.
    fn end_move(&mut self, stop: bool, t: &dyn Walkable) {
        self.mv = None;
        if stop && self.moving {
            self.end_path(false, t);
        }
    }

    /// `Actor_PlanMove` `0x141e42650`.
    fn plan_move(&mut self, t: &dyn Walkable) {
        let Some(m) = self.mv else { return };
        self.plan_timer = PLAN_TIMER;
        self.in_plan = true;
        self.follow_path(m.target, t);
        self.in_plan = false;
    }

    /// `Actor_ReaimWasdWalk` `0x141e50320`.
    fn reaim(&mut self, t: &dyn Walkable) {
        let Some(m) = self.mv else { return };
        if m.from_server || self.in_plan || self.roll.is_some() {
            return;
        }
        let goal = self.axes_goal();
        self.mv = Some(MoveAction { target: goal, ..m });
        self.plan_move(t);
    }

    /// `grid + (i32)floorf((f32)axes * 0.092f)` per axis.
    fn axes_goal(&self) -> Cell {
        (
            self.grid.0 + from_world(self.axes.0 as f32),
            self.grid.1 + from_world(self.axes.1 as f32),
        )
    }

    /// `Actor_StartWasdWalk` `0x141e4fb40`.
    fn start_wasd_walk(&mut self, t: &dyn Walkable) {
        if self.in_plan || self.sprint_end.is_some() {
            return;
        }
        if self.axes == (0, 0) {
            if self.mv.is_some_and(|m| !m.from_server) {
                self.end_move(true, t);
            }
            return;
        }
        if self.roll.is_some() {
            return;
        }
        let goal = self.axes_goal();
        let from_server = self.mv.is_some_and(|m| m.from_server);
        self.mv = Some(MoveAction { target: goal, from_server });
        self.plan_move(t);
    }

    /// The Actor tick `0x141e42a00`, the steps that move the player.
    pub fn actor_tick(&mut self, dt: f32, t: &dyn Walkable) {
        if self.roll.is_none()
            && let Some((start, target)) = self.queued_move.take()
        {
            self.on_start_move(start, target, t);
        }
        if self.axes != (0, 0) {
            if self.mv.is_none() {
                self.start_wasd_walk(t);
            } else if self.plan_timer <= 0.0 {
                self.reaim(t);
            }
        }
        self.roll_tick(dt, t);
        self.sprint_end_check(t);
        self.plan_timer = (self.plan_timer - dt).max(0.0);
        self.frames += 1;
    }

    // ---- Messages ----

    /// `0x64`, case 100 of `0x1421497c0`.
    pub fn on_move_ack(&mut self, dir: (i32, i32), cell: Cell, t: &dyn Walkable) {
        if dir != (0, 0) && cell != self.grid {
            self.snap(cell);
        }
        if dir == (0, 0) && self.axes != (0, 0) && !self.axes_fresh {
            return;
        }
        self.axes = dir;
        self.axes_fresh = true;
        self.start_wasd_walk(t);
    }

    /// `0x1c8` of a Move: `Actor_StartQueuedAction` `0x141e40b50`, Move branch.
    pub fn on_start_move(&mut self, start: Cell, target: Cell, t: &dyn Walkable) {
        // A running roll keeps slot 0: the Move stays queued (`A + 0x248`) until the Actor tick after
        // the roll's end; its start cell snaps then (`Actor_InstallQueuedMove`).
        if self.roll.is_some() {
            self.queued_move = Some((start, target));
            return;
        }
        let d = (self.grid.0 - target.0, self.grid.1 - target.1);
        if (((d.0 * d.0 + d.1 * d.1) as f32).sqrt()) < 9.0 {
            return;
        }
        if self.sprint_end.is_some() {
            return;
        }
        self.end_move(false, t);
        if start != self.grid {
            self.snap(start);
        }
        self.mv = Some(MoveAction { target, from_server: true });
        self.plan_move(t);
    }

    /// `0x1c8` of a skill that holds slot 0 (an attack, an interaction): the new action replaces a
    /// running roll and ends a running Move. The skills' own root motion is not ported: the server
    /// sends no skills yet (the offline replay follows the client's log while one runs).
    pub fn on_start_other(&mut self, start: Cell, t: &dyn Walkable) {
        if start != self.grid {
            self.snap(start);
        }
        self.action_slow = Some(self.frames);
        self.end_move(true, t);
        if self.roll.is_some() {
            self.roll = None;
            self.stop_next_tick(t);
            self.orient = 1;
            self.line.curve.clear();
        }
    }

    /// `0x1af`: `Player_ApplyServerSkillTarget` `0x141dd4550` -> `0x141e5b250`. Before the target
    /// window it only stores the action's target; inside it `0x14197cf50` keeps the target relative
    /// to the grid for the action's update, unless the movement vector took over (`act + 0x24d`).
    pub fn on_skill_target(&mut self, target: Cell) {
        let g = self.grid;
        if let Some(r) = &mut self.roll {
            if !r.target_update {
                r.target = target;
            } else if !r.axes_aimed {
                r.pending_target = Some((target.0 - g.0, target.1 - g.1));
            }
        }
    }

    /// `0x2e7`: the sprint key (`0x141e5c1f0`).
    pub fn on_sprint_key(&mut self, held: bool) {
        self.sprint_key = held;
    }

    /// `0x1d3` `movement_velocity_+permyriad`. A running line takes a new speed at the next plan.
    pub fn on_permyriad(&mut self, v: i32) {
        self.permyriad = v;
    }

    pub fn on_must_sprint(&mut self, v: bool) {
        self.must_sprint = v;
    }

    /// `0x1d3` `is_sprinting`.
    pub fn on_is_sprinting(&mut self, v: bool) {
        self.is_sprinting = v;
    }

    /// The animation event `0xb91f`: `DodgeRoll_OnAnimationEvent` `0x141aa89d0`, with the key held
    /// the sprint child (`DodgeRoll_StartSprint` `0x141aa92e0`) and the switch of curve
    /// (`DodgeRollMove_SwitchAnimation` `0x141b8cd50`, `DodgeRollMove_ReloadCurve` `0x141b8d440`).
    fn roll_event(&mut self) {
        if !self.sprint_key {
            return;
        }
        if let (Some(r), Some(cs)) = (&mut self.roll, self.curves.clone()) {
            r.sprints = true;
            r.opt_len = Some(ROLL_SPRINT_LINE);
            let curve = if r.backward { cs.roll_back_sprint.clone() } else { cs.roll_sprint.clone() };
            let (e, l) = (r.elapsed, r.length);
            self.line.curve.load(curve, 0.0, l);
            self.line.curve.set_time(e, l);
            // A backward roll that goes on sprinting turns around (`act + 0x461`: stance `Standing`).
            if r.backward {
                self.orient = 1;
                self.facing = wrap(self.facing + PI);
            }
        }
    }

    // ---- Roll (`docs/re/movement-roll.md`) ----

    /// `0x1c8` of a roll: `DodgeRoll_OnStart` `0x141aa8500`, `DodgeRoll_InitHeading` `0x141b8bda0`,
    /// `DodgeRollMove_Start` `0x141b8bfa0`.
    pub fn on_start_roll(&mut self, start: Cell, target: Cell, travel: f32, t: &dyn Walkable) {
        // The roll is the running action now: its own factor (1, no ramp) replaces an attack's.
        // Measured on one sample only: the attack a roll cut short (f4021) showed only the roll's
        // own steps, and the walk after it was at full speed.
        self.action_slow = None;
        self.speed_factor = 1.0;
        let Some(cs) = self.curves.clone() else { return };
        // `Actor_InstallQueuedMove` `0x141e41c60` snaps, then `0x141e4ea40` ends a running Move with a
        // stop (`Actor_EndMoveAction(A, 1)`) before the action's start, so the path's end does not
        // clear the roll's curve.
        if start != self.grid {
            self.snap(start);
        }
        // `0x141e4ea40`: an action whose vfunc `+0x340` answers true faces its target before anything
        // else, so a key-held roll away from the cursor is a backward roll.
        if target != self.grid {
            self.facing = angle((target.0 - self.grid.0) as f32, (target.1 - self.grid.1) as f32);
        }
        self.end_move(true, t);
        let heading = if self.axes != (0, 0) {
            angle(from_world(self.axes.0 as f32) as f32, from_world(self.axes.1 as f32) as f32)
        } else if target != self.grid {
            // TEST: the first heading of a cursor roll.
            let d = ((target.0 - self.grid.0) as f32, (target.1 - self.grid.1) as f32);
            let l = (d.0 * d.0 + d.1 * d.1).sqrt();
            angle(round(d.0 / l * 3.0) as f32, round(d.1 / l * 3.0) as f32)
        } else {
            self.facing
        };
        let mut d = wrap(heading - self.facing);
        if d > PI {
            d -= TAU;
        }
        let backward = self.sprint_end.is_none() && d.abs() >= HALF_PI;
        let curve = if backward { cs.roll_back.clone() } else { cs.roll.clone() };
        let length = curve.length;
        // The new action ends the clip and its move event: the roll's line starts on the grid
        // (seg1 f3578: y exactly 635).
        if self.sprint_end.take().is_some() {
            self.stop_move();
            self.line.acc = 0.0;
            self.line.pending = 0.0;
        }
        self.roll = Some(Roll {
            elapsed: 0.0,
            length,
            rate: 1.0,
            heading,
            backward,
            // `sqrt(1 + stat(0xec) / 10000) * stat(0x57fa)`.
            total: ((1.0 + self.permyriad as f32 / 10000.0).sqrt() * travel) as i32,
            waiting: false,
            axes_aimed: false,
            opt_len: None,
            sprints: false,
            event_done: false,
            target_update: false,
            pending_target: None,
            target,
        });
        self.line.curve.load(curve, 0.0, length);
        self.orient = if backward { 2 } else { 1 };
        let (goal, _) = self.roll_goal(t);
        let target = self.roll_target(goal);
        let speed = self.roll_speed();
        if goal != self.grid {
            let target = self.clip_to_walkable(target, t);
            self.move_to_waypoint(target, speed, false, t);
        }
    }

    /// The target block of the action's update `0x141974090` (`0x141974a90`): the wanted target
    /// `grid + act+0x250` is at most `dt * act+0x244` from the facing; vfunc `+0x130` `0x141b8c6a0`
    /// aims the heading at the result. The roll's vfunc `+0x138` is false: the facing stays.
    fn roll_turn_to_target(&mut self, dt: f32) {
        let g = self.grid;
        let rate = self.curves.as_ref().map_or(0.0, |c| c.roll_events.target_turn_rate);
        let (moving, stop) = (self.line.thr != 0.0, self.stop_pending);
        let sign = if self.orient == 2 { -1 } else { 1 };
        let facing = self.facing;
        let r = self.roll.as_mut().unwrap();
        let Some(off) = r.pending_target else { return };
        let mut e = (g.0 + off.0, g.1 + off.1);
        if e == r.target && (!moving || stop) {
            return;
        }
        let want = angle((sign * off.0) as f32, (sign * off.1) as f32);
        let mut d = (want - facing) % TAU;
        if d < -PI {
            d += TAU;
        } else if d > PI {
            d -= TAU;
        }
        let max = dt * rate;
        if max < d.abs() {
            let a = facing + d.clamp(-max, max);
            let len = ((off.0 * off.0 + off.1 * off.1) as f32).sqrt();
            let v = rot(0.0, -len, a);
            e = (g.0 + sign * round(v.0), g.1 + sign * round(v.1));
        }
        r.target = e;
        if e != g {
            r.heading = angle((e.0 - g.0) as f32, (e.1 - g.1) as f32);
        }
    }

    /// `DodgeRollMove_DistanceLeft` `0x141b8cf70`.
    fn roll_left(&self) -> f32 {
        let r = self.roll.as_ref().unwrap();
        let c = &self.line.curve;
        let v = c.value(c.t);
        let f = ((v - c.c0) / (c.c_end - c.c0)).clamp(0.0, 1.0);
        (1.0 - f) * r.total as f32
    }

    /// `DodgeRollMove_Speed` `0x141b8d770`.
    fn roll_speed(&self) -> f32 {
        let r = self.roll.as_ref().unwrap();
        if r.rate == 0.0 {
            return 0.0;
        }
        let left_t = r.length / r.rate - r.elapsed / r.rate;
        if left_t == 0.0 { 0.0 } else { self.roll_left() / left_t }
    }

    /// `DodgeRollMove_Goal` `0x141b8d1f0`: the first waypoint towards the point `left` ahead.
    fn roll_goal(&self, t: &dyn Walkable) -> (Cell, bool) {
        let heading = self.roll.as_ref().unwrap().heading;
        let v = rot(0.0, -self.roll_left().max(10.0), heading);
        let g = if self.on_line() {
            let p = self.line.point();
            (round(p.0 + v.0), round(p.1 + v.1))
        } else {
            (self.grid.0 + round(v.0), self.grid.1 + round(v.1))
        };
        let path = self.search(g, t);
        if path.len() < 2 {
            return (self.grid, false);
        }
        (path[path.len() - 2], false)
    }

    /// `DodgeRollMove_Target` `0x141b8d000`.
    fn roll_target(&self, goal: Cell) -> Cell {
        let left = self.roll_left();
        if self.on_line() {
            let p = self.line.point();
            let a = angle(goal.0 as f32 - p.0, goal.1 as f32 - p.1);
            let v = rot(0.0, -left, a);
            (round(p.0 + v.0), round(p.1 + v.1))
        } else {
            let a = angle((goal.0 - self.grid.0) as f32, (goal.1 - self.grid.1) as f32);
            let v = rot(0.0, -left, a);
            (self.grid.0 + round(v.0), self.grid.1 + round(v.1))
        }
    }

    /// The Bresenham walk of `0x141b8fe20` and `0x141b90060`.
    fn bresenham(from: Cell, to: Cell, mut f: impl FnMut(Cell) -> bool) {
        let d = (to.0 - from.0, to.1 - from.1);
        let (major, minor) = (d.0.abs().max(d.1.abs()), d.0.abs().min(d.1.abs()));
        let x_major = d.0.abs() >= d.1.abs();
        let s = (d.0.signum(), d.1.signum());
        let mut err = -major / 2;
        let mut p = from;
        for i in 0..=major {
            if !f(p) {
                return;
            }
            if i == major {
                return;
            }
            err += minor;
            if x_major { p.0 += s.0 } else { p.1 += s.1 }
            if err > 0 {
                if x_major { p.1 += s.1 } else { p.0 += s.0 }
                err -= major;
            }
        }
    }

    /// `DodgeRollMove_ClipToWalkable` `0x141b90060`.
    fn clip_to_walkable(&self, target: Cell, t: &dyn Walkable) -> Cell {
        let mut out = target;
        Self::bresenham(self.grid, target, |p| {
            if t.layer(0, p.0, p.1) < 1 {
                return false;
            }
            out = p;
            true
        });
        out
    }

    /// `DodgeRollMove_Tick` `0x141b8c720`, and the end of the action.
    fn roll_tick(&mut self, dt: f32, t: &dyn Walkable) {
        // Action update (vfunc `+0x1c0` `0x141a3b010` -> `0x141974090` -> `0x14197d060`), with the stat
        // `dynamic_target_updating_prioritises_movement_vector`: the target follows the axes.
        if self.roll.as_ref().is_some_and(|r| r.target_update) {
            if self.axes != (0, 0) {
                let goal = self.axes_goal();
                let g = self.grid;
                let r = self.roll.as_mut().unwrap();
                r.pending_target = Some((goal.0 - g.0, goal.1 - g.1));
                r.axes_aimed = true;
            }
            self.roll_turn_to_target(dt);
        }
        let Some(r) = &self.roll else { return };
        if r.waiting {
            if self.line.thr != 0.0 {
                return;
            }
            let (curve, e, l) = (self.line.curve.curve.clone(), r.elapsed, r.length);
            if let Some(c) = curve {
                self.line.curve.load(c, 0.0, l);
                self.line.curve.set_time(e, l);
            }
            self.roll.as_mut().unwrap().waiting = false;
        }
        if self.line.curve.curve.is_some() && self.roll_left() > 1.0 {
            self.roll_step(t);
        }
    }

    /// `DodgeRollMove_Step` `0x141b8c7f0`.
    fn roll_step(&mut self, t: &dyn Walkable) {
        let (goal, _) = self.roll_goal(t);
        let target = self.roll_target(goal);
        let speed = self.roll_speed();
        let (e, l, opt) = {
            let r = self.roll.as_ref().unwrap();
            (r.elapsed, r.length, r.opt_len)
        };
        if speed == 0.0 {
            if self.line.thr != 0.0 {
                self.roll.as_mut().unwrap().waiting = true;
                self.stop_with_callbacks(t);
            }
            return;
        }
        self.line.curve.set_time(e, l);
        // `DodgeRollMove_CheckClearCells` `0x141b8fe20`.
        let mut counter = 2;
        let grid = self.grid;
        Self::bresenham(grid, target, |p| {
            if t.layer(0, p.0, p.1) < 1 {
                return false;
            }
            if p != grid {
                counter -= 1;
                if counter < 1 {
                    return false;
                }
            }
            true
        });
        if counter > 0 {
            return;
        }
        let mut target = self.clip_to_walkable(target, t);
        let mut k = 0.0;
        if let Some(n) = opt {
            let d = (target.0 - grid.0, target.1 - grid.1);
            k = speed / ((d.0 * d.0 + d.1 * d.1) as f32).sqrt();
            let a = angle((goal.0 - grid.0) as f32, (goal.1 - grid.1) as f32);
            let v = rot(0.0, -(n as f32), a);
            target = (grid.0 + round(v.0), grid.1 + round(v.1));
        }
        self.move_to_waypoint(target, speed, false, t);
        if opt.is_some() {
            self.line.opt_rate = k;
        }
    }

    /// `DodgeRoll_OnEnd` `0x141aa8ef0`.
    fn roll_end(&mut self, t: &dyn Walkable) {
        let r = self.roll.take().unwrap();
        self.stop_next_tick(t);
        self.orient = 1;
        if r.sprints {
            self.sprint_start(t);
        } else if r.backward {
            // `DodgeRoll_OnEnd`, with `act + 0x461` (stance `Standing`).
            self.facing = wrap(self.facing + PI);
        }
        // The end of the action in the Animated tick: a walk on the held axes starts at once (two
        // plans in the frame of a sprint start, seen in `PATHPLAN`: each makes its sprint turn).
        if self.axes != (0, 0) && self.mv.is_none() {
            self.start_wasd_walk(t);
        }
    }

    // ---- Sprint (`docs/re/movement-sprint.md`) ----

    /// `SprintPart_Start` `0x141b77110`.
    fn sprint_start(&mut self, t: &dyn Walkable) {
        self.sprint = true;
        self.level = self.sprint_level;
        if self.continues_with_move_input && self.axes == (0, 0) {
            self.axes = (
                (self.facing.sin() * 180.0) as i32,
                (-self.facing.cos() * 180.0) as i32,
            );
            self.axes_fresh = true;
            self.start_wasd_walk(t);
        }
    }

    /// `SprintPart_EndCheck` `0x141b76da0`.
    fn sprint_end_check(&mut self, t: &dyn Walkable) {
        if !self.sprint {
            return;
        }
        if !self.is_sprinting {
            self.sprint_part_end(false, t);
            return;
        }
        if self.roll.is_none() && self.mv.is_none() && !self.moving {
            self.bonked = true;
            self.sprint_part_end(true, t);
            return;
        }
        if self.must_sprint {
            return;
        }
        let keeps = self.sprint_key || (self.continues_with_move_input && self.axes != (0, 0));
        if !keeps {
            self.sprint_part_end(false, t);
        }
    }

    /// `SprintPart_End` `0x141b773a0` and the play of the clip `0x141e57b80`.
    fn sprint_part_end(&mut self, bonk: bool, t: &dyn Walkable) {
        self.sprint = false;
        self.end_move(false, t);
        self.stop_with_callbacks(t);
        self.level = 0;
        if self.roll.is_some() {
            return;
        }
        let v = self.speed() * CELL;
        let rate = v / (SPRINT_END_BASE_SPEED * self.offset_scale);
        if rate > 0.0 {
            self.sprint_end = Some(SprintEndAnim {
                left: SPRINT_END_MOVE_TIME,
                rate,
                coast_due: !bonk,
            });
        }
    }

    /// `MoveEvent_Start` `0x141f5f650`, `Positioned_RootOffsetToCell` `0x141de6030`.
    fn start_coast(&mut self, rate: f32, t: &dyn Walkable) {
        let Some(cs) = self.curves.clone() else { return };
        let disp = cs.sprint_end.value(SPRINT_END_MOVE_TIME) - cs.sprint_end.value(0.0);
        let r = rot(0.0, -disp * self.offset_scale, self.facing);
        let target = (
            from_world(world(self.grid.0) + r.0),
            from_world(world(self.grid.1) + r.1),
        );
        if target == self.grid {
            return;
        }
        let d = (target.0 - self.grid.0, target.1 - self.grid.1);
        let dist = ((d.0 * d.0 + d.1 * d.1) as f32).sqrt();
        let speed = dist / (SPRINT_END_MOVE_TIME / rate);
        let end = self.clip_to_walkable(target, t);
        // `0x141f63640`: fewer than 2 cells to go beyond the start cell is no move at all.
        let e = (end.0 - self.grid.0, end.1 - self.grid.1);
        if e.0.abs().max(e.1.abs()) >= 2 {
            // `MoveEvent_Start`: `Positioned_SetMoveCurve` before `MoveEvent_MovePositioned`.
            // `0x141f62210`: a clipped end ends the curve where the clip has moved that distance,
            // `|cell delta| * 10.87 / |ev+0x2c| + |curve(state elapsed +0x24)|`, then the inverse of
            // the curve (`0x141ecd760`). `ev+0x2c` is the event's data, 1. The event fires in the
            // clip's first Animated tick, one step into it.
            let e_time = if end == target {
                SPRINT_END_MOVE_TIME
            } else {
                let moved = ((e.0 * e.0 + e.1 * e.1) as f32).sqrt() * CELL;
                cs.sprint_end.time_of(moved + cs.sprint_end.value(0.0).abs())
            };
            self.line.curve.load(cs.sprint_end.clone(), 0.0, e_time);
            let orient = std::mem::replace(&mut self.orient, 0);
            self.move_to_waypoint(end, speed, false, t);
            self.orient = orient;
        }
    }
}
