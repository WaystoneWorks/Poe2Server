//! A port of the client's WASD movement: line, Positioned tick, plans, sprint turn and roll
//! (docs/re/wasd-movement.md, every rule and address there). In grid cells, cell centres on whole
//! numbers. One frame: [`ClientMotion::positioned_tick`], then the frame's messages, then
//! [`ClientMotion::actor_tick`].

use crate::clientpath::{self, Params};
use crate::movement::Walkable;
use std::f32::consts::{PI, TAU};
use std::sync::Arc;

pub type Cell = (i32, i32);

/// `0x142193270`: input units to cells.
const PER_UNIT: f32 = 0.092;
/// `0x142193230`: cells to world units.
const UNITS_PER_CELL: f32 = 10.869565;
/// `FUN_141e42650`: seconds between plans of a player's Move.
const PLAN_PERIOD: f32 = 0.05;
/// `0x141dcc6a0`: goals at least this far (squared) are pulled in to 126 cells.
const GOAL_LIMIT_SQ: i64 = 0x4840;
const GOAL_CLAMP: f32 = 126.0;
/// `0x141dcc460`: the line of a turning sprint ends this many cells along the turned facing.
const SPRINT_TURN_AHEAD: f32 = 180.0;
/// `FUN_141b8c7f0`: a roll stops steering when its target is this close (`FUN_140132800`,
/// measured: between 1.41 and 2 by the `LINE` log).
const ROLL_STOP_DISTANCE: f32 = 1.6;
/// `FUN_141b8d1f0`: a roll's search looks at least this far ahead.
const ROLL_MIN_LOOKAHEAD: f32 = 10.0;
/// `SprintEnd` is `speed = 600` in `character.act` (`0x141e56ae0`).
const SPRINT_END_ANIM_SPEED: f32 = 600.0;

/// A movement curve of an animation (`.amd`: duration and one position per frame).
#[derive(Debug, Clone, PartialEq)]
pub struct Curve {
    /// Seconds.
    pub length: f32,
    pub positions: Arc<[f32]>,
}

impl Curve {
    pub fn new(length: f32, positions: Vec<f32>) -> Self {
        Self {
            length,
            positions: positions.into(),
        }
    }

    /// `FUN_141ecccc0`.
    fn value(&self, t: f32) -> f32 {
        let n = self.positions.len();
        if n < 2 || t < 0.0 {
            return 0.0;
        }
        let loops = (t / self.length).floor();
        let frame = (t % self.length) / self.length * (n - 1) as f32;
        let i = (frame.floor() as usize).min(n - 1);
        let next = (i + 1).min(n - 1);
        let first = self.positions[0];
        let last = self.positions[n - 1];
        (self.positions[next] - self.positions[i]) * (frame - i as f32)
            + self.positions[i]
            + (last - first) * loops
    }
}

/// The curves of the character's dodge roll and sprint end (`basedodgeroll`, `basesprint`).
#[derive(Debug, Clone, PartialEq)]
pub struct MotionCurves {
    pub roll: Curve,
    pub roll_back: Curve,
    pub roll_sprint: Curve,
    pub roll_back_sprint: Curve,
    pub sprint_end: Curve,
}

/// A curve in use (Positioned `+0x2a8`, `L + 0xa0`).
#[derive(Debug, Clone, PartialEq)]
struct CurveRun {
    curve: Curve,
    /// `[8]` time.
    t: f32,
    /// `[7]` time from the last restart to the end.
    span: f32,
    /// `[1]` value at the last restart.
    from: f32,
    /// `[0]` value left from the last restart.
    range: f32,
    /// `[3]`, `[4]` values at the start and the end (fixed at load).
    first: f32,
    last: f32,
}

impl CurveRun {
    /// `FUN_141ecc540`.
    fn load(curve: Curve) -> Self {
        let first = curve.value(0.0);
        let last = curve.value(curve.length);
        let mut run = Self {
            span: 0.0,
            from: 0.0,
            range: 0.0,
            t: 0.0,
            first,
            last,
            curve,
        };
        run.restart(0.0);
        run
    }

    /// `FUN_141ecc710`.
    fn restart(&mut self, t: f32) {
        self.t = t;
        self.span = self.curve.length - t;
        self.from = self.curve.value(t);
        self.range = self.curve.value(self.curve.length) - self.from;
    }

    /// `FUN_141ecc780`: the share of the remaining range passed.
    fn advance(&mut self, dt: f32, rate: f32) -> f32 {
        let old = self.curve.value(self.t);
        self.t += self.span * rate * dt;
        if self.range == 0.0 {
            return 0.0;
        }
        (self.curve.value(self.t) - old) / self.range
    }

    /// Share of the curve passed, `0..=1` (`FUN_141b8cf70`).
    fn done(&self) -> f32 {
        ((self.curve.value(self.t) - self.first) / (self.last - self.first)).clamp(0.0, 1.0)
    }
}

/// The line a Positioned walks (`L = Positioned + 0x208`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct Line {
    s: Cell,
    p: (f32, f32),
    q: Cell,
    e: Cell,
    flag: bool,
    swap: bool,
    sign: i32,
    slope: f32,
    end_major: i32,
    major: i32,
    minor: i32,
    minor_acc: f32,
    distance_mode: bool,
    len: f32,
    t: f32,
    inv_t: f32,
    /// `+0x78`, written by the roll step.
    rate: f32,
    elapsed: f32,
    distance: f32,
    thr: f32,
    acc: f32,
    pending: f32,
    step_time: f32,
    /// `+0x98`.
    pending_scale: f32,
}

impl Line {
    fn cur(&self) -> Cell {
        if self.swap {
            (self.minor, self.major)
        } else {
            (self.major, self.minor)
        }
    }

    fn set_cur(&mut self, c: Cell) {
        (self.major, self.minor) = if self.swap { (c.1, c.0) } else { (c.0, c.1) };
    }

    /// `FUN_141ec83d0`.
    fn point(&self) -> (f32, f32) {
        let k = if self.distance_mode {
            if self.len > 0.0 { self.distance / self.len } else { 1.0 }
        } else if self.t > 0.0 {
            self.elapsed / self.t
        } else {
            1.0
        };
        let k = k.clamp(0.0, 1.0);
        (
            self.p.0 + (self.e.0 as f32 - self.p.0) * k,
            self.p.1 + (self.e.1 as f32 - self.p.1) * k,
        )
    }

    fn running(&self) -> bool {
        self.len != 0.0
    }

    /// `FUN_141ec86a0`.
    fn setup(&mut self, s: Cell, e: Cell, speed: f32, distance_mode: bool) {
        let sf = (s.0 as f32, s.1 as f32);
        let ef = (e.0 as f32, e.1 as f32);
        let mut running = self.running() && self.cur() == s;
        let mut p = sf;
        if running {
            let old = self.point();
            if old != ef && (sf.0 - old.0).powi(2) + (sf.1 - old.1).powi(2) < 4.0 {
                p = old;
            } else {
                running = false;
            }
        }
        let dir = (ef.0 - p.0, ef.1 - p.1);
        let (q, flag) = if running {
            let t = (dir.0 * (sf.0 - p.0) + dir.1 * (sf.1 - p.1)) / (dir.0 * dir.0 + dir.1 * dir.1);
            let q = (
                (p.0 + dir.0 * t + 0.5).floor() as i32,
                (p.1 + dir.1 * t + 0.5).floor() as i32,
            );
            (q, q != s)
        } else {
            (s, false)
        };
        let (dx, dy) = ((e.0 - q.0).abs(), (e.1 - q.1).abs());
        let swap = dx < dy || (dx == dy && running && dir.0.abs() < dir.1.abs());
        let pick = |v: (f32, f32)| if swap { (v.1, v.0) } else { v };
        let picki = |v: Cell| if swap { (v.1, v.0) } else { v };
        let (dir_major, dir_minor) = pick(dir);
        let (p_major, p_minor) = pick(p);
        let _ = p_major;
        let (e_major, _) = picki(e);
        let (q_major, _) = picki(q);
        let len = (dir.0 * dir.0 + dir.1 * dir.1).sqrt();
        let t = len / speed;
        let d = dir_major.abs();
        let thr = if distance_mode { len / d } else { t / d };
        let slope = dir_minor / d;
        let k = (e_major - q_major).abs() as f32 - d;
        let pending = self.pending;
        *self = Self {
            s,
            p,
            q,
            e,
            flag,
            swap,
            sign: (e_major - q_major).signum(),
            slope,
            end_major: e_major,
            major: 0,
            minor: 0,
            minor_acc: p_minor - slope * k + if slope > 0.0 { 0.49 } else { 0.5 },
            distance_mode,
            len,
            t,
            inv_t: 1.0 / t,
            rate: 0.0,
            elapsed: 0.0,
            distance: 0.0,
            thr,
            acc: thr * k,
            pending,
            step_time: t / d,
            pending_scale: 1.0,
        };
        self.set_cur(s);
    }

    /// `FUN_141ec82e0`.
    fn advance(&mut self, dt: f32, curve: Option<&mut CurveRun>) {
        let mut x = dt + self.pending;
        self.pending = x;
        self.elapsed += x;
        if self.distance_mode {
            let rate = if self.rate > 0.0 { self.rate } else { self.inv_t };
            let share = curve.map_or(0.0, |c| c.advance(x, rate));
            self.pending_scale = if share > 0.0 { rate * self.pending / share } else { 1.0 };
            x = share * self.len;
            if self.rate > 0.0 {
                x *= self.inv_t / self.rate;
            }
        }
        self.pending = 0.0;
        self.acc += x;
        self.distance += x;
    }

    /// Whether the tick loop steps again.
    fn wants_step(&self) -> bool {
        self.thr != 0.0 && (self.thr <= self.acc || (self.flag && self.acc > 0.0))
    }

    /// `FUN_141ec8cb0`: one step; returns the cell and whether the line is done.
    fn step(&mut self) -> (Cell, bool) {
        if self.flag {
            let mut c = self.cur();
            c.0 += (self.q.0 - c.0).signum();
            c.1 += (self.q.1 - c.1).signum();
            self.set_cur(c);
            if c == self.q {
                self.flag = false;
                if self.major == self.end_major {
                    self.acc -= if self.distance_mode { self.len } else { self.t };
                }
            }
        } else {
            self.major += self.sign;
            self.acc -= self.thr;
            self.minor_acc += self.slope;
            self.minor = self.minor_acc.floor() as i32;
        }
        let cell = self.cur();
        let done = !self.flag && self.major == self.end_major;
        if done {
            self.finish();
        }
        (cell, done)
    }

    /// `FUN_141ec8f00`.
    fn finish(&mut self) {
        self.pending = if self.distance_mode && self.thr != 0.0 {
            self.pending_scale * self.step_time * self.acc / self.thr
        } else {
            self.acc
        }
        .max(0.0);
        self.thr = 0.0;
        self.len = 0.0;
    }

    /// `FUN_141de5f50` on the line.
    fn kill(&mut self) {
        self.finish();
        self.acc = 0.0;
        self.pending = 0.0;
    }
}

/// The fields of the DLL's `LINE` log. `p`, `q`, `e`, `cur` are `(major, minor)`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LoggedLine {
    pub s: Cell,
    pub p: (f32, f32),
    pub q: Cell,
    pub e: Cell,
    pub cur: Cell,
    pub flag: bool,
    pub swap: bool,
    pub sign: i32,
    pub slope: f32,
    pub minacc: f32,
    pub len: f32,
    pub t: f32,
    pub el: f32,
    pub thr: f32,
    pub acc: f32,
    pub pend: f32,
}

/// A running roll (`FUN_141b8c720`).
#[derive(Debug, Clone, PartialEq)]
struct Roll {
    /// `+0x40`, facing convention.
    heading: f32,
    /// `+0x4c`.
    total: i32,
    /// Animation time, seconds.
    elapsed: f32,
    /// `+0x3d` steering stays on until the goal is close.
    steering: bool,
    /// `+0x44`, `+0x48`: the optional length of the line, cells.
    extend: Option<f32>,
    /// The heading follows the keys, not the cursor.
    keys: bool,
    /// `+0x3f`: at least 90 degrees from the facing: `_bwd_` animation, orientation mode 2.
    backward: bool,
    /// Clearance level of the action's search (`+0x20`, `FUN_141b8d1f0`), fixed at its start: the
    /// sprint arriving during the roll does not change it.
    level: i32,
}

/// The Move slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Walk {
    None,
    /// WASD: the goal comes from the axes at each plan.
    Axes,
    /// Click-to-move.
    Target(Cell),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClientMotion {
    /// Positioned `+0x444`.
    grid: Cell,
    line: Line,
    /// Positioned `+0x44c`.
    facing: f32,
    /// Actor `+0x400`.
    axes: (i32, i32),
    /// Actor `+0x3dc`.
    plan_timer: f32,
    walk: Walk,
    /// A `0x64 (0, 0)` ended the walk: the line runs through the next frame's Positioned tick,
    /// then it stops. Actor ticks left.
    ending: u8,
    /// The walk's path in walking order, and the next entry.
    path: Vec<Cell>,
    next: usize,
    /// Pathfinding `+0x580`.
    turn_factor: f32,
    roll: Option<Roll>,
    curve: Option<CurveRun>,
    /// The animation's coast line runs ([`ClientMotion::start_coast`]).
    coasting: bool,
    pub curves: Option<Arc<MotionCurves>>,
    /// The class's `offset_scale` (`.ao`), scales root motion and the animation's rate.
    pub offset_scale: f32,
    /// Seconds of `SprintEnd` left. `Actor_StartWasdWalk` starts nothing while it plays.
    end_anim: f32,
    /// The rate of the `SprintEnd` that started in the last Actor tick: its move event comes in
    /// the next frame's Animated tick (`0x141da76c0`).
    coast_due: Option<f32>,
    /// The sprint part is on (`0x141b77110` ran, `0x141b773a0` not yet).
    sprint_on: bool,
    /// The buff `must_sprint`: key state does not end the sprint while it is on.
    pub must_sprint: bool,
    /// The speed when the frame started: the stats of a frame do not reach the `SprintEnd` rate
    /// of that frame (`LINE`: coast T after a batch with the sprint-off stats).
    frame_speed: f32,
    /// Cells per second: base speed, attack factor and the sprint's stats.
    pub speed: f32,
    pub factor: f32,
    pub scale: f32,
    /// The stat `is_sprinting`.
    pub is_sprinting: bool,
    /// Stat `dodge_roll_sprint_turn_rate`, degrees per plan.
    pub turn_rate: f32,
    /// Clearance level of the walk's searches.
    pub level: i32,
    /// Plans made so far, for tests and logs.
    pub plans: u64,
}

fn round(x: f32) -> i32 {
    (x + 0.5).floor() as i32
}

fn floor_goal(v: i32) -> i32 {
    (v as f32 * PER_UNIT).floor() as i32
}

fn world(c: f32) -> f32 {
    (c + 0.5) * UNITS_PER_CELL
}

fn facing_of(dx: f32, dy: f32) -> f32 {
    (dy.atan2(dx) + PI / 2.0).rem_euclid(TAU)
}

impl ClientMotion {
    pub fn at_cell(cell: Cell, speed: f32) -> Self {
        Self {
            grid: cell,
            line: Line::default(),
            facing: 0.0,
            axes: (0, 0),
            plan_timer: 0.0,
            walk: Walk::None,
            ending: 0,
            path: Vec::new(),
            next: 0,
            turn_factor: 0.99,
            roll: None,
            curve: None,
            coasting: false,
            curves: None,
            offset_scale: 1.0,
            end_anim: 0.0,
            coast_due: None,
            sprint_on: false,
            must_sprint: false,
            frame_speed: speed,
            speed,
            factor: 1.0,
            scale: 1.0,
            is_sprinting: false,
            turn_rate: 5.0,
            level: 0,
            plans: 0,
        }
    }

    pub fn grid(&self) -> Cell {
        self.grid
    }

    /// The position on the current line, in cell-centred coordinates.
    pub fn point(&self) -> (f32, f32) {
        if self.line.running() {
            self.line.point()
        } else {
            (self.grid.0 as f32, self.grid.1 as f32)
        }
    }

    /// Line state for logs: time, elapsed, acc, length.
    pub fn line_state(&self) -> (f32, f32, f32, f32) {
        (self.line.t, self.line.elapsed, self.line.acc, self.line.len)
    }

    /// End cell of the running line.
    pub fn line_end(&self) -> Option<Cell> {
        self.line.running().then_some(self.line.e)
    }

    pub fn axes(&self) -> (i32, i32) {
        self.axes
    }

    pub fn is_walking(&self) -> bool {
        self.walk != Walk::None
    }

    pub fn is_rolling(&self) -> bool {
        self.roll.is_some()
    }

    /// A line runs.
    pub fn is_moving(&self) -> bool {
        self.line.running()
    }

    fn move_speed(&self) -> f32 {
        self.speed * self.factor * self.scale
    }

    /// Whether the entity may stand on `cell` (`FUN_141ddcfc0`, walkability only: level 0).
    fn free(terrain: &dyn Walkable, cell: Cell) -> bool {
        if let Some((w, h)) = terrain.bounds()
            && (cell.0 < 0 || cell.1 < 0 || cell.0 >= w || cell.1 >= h)
        {
            return false;
        }
        terrain.nibble(cell.0, cell.1) >= 1
    }

    // --- Positioned ---

    /// `FUN_141b87580`: a line runs and its current cell is the grid.
    fn on_line(&self) -> bool {
        self.line.running() && self.line.cur() == self.grid
    }

    /// The frame's components before the Actor: Positioned, then Animated.
    pub fn positioned_tick(&mut self, dt: f32, terrain: &dyn Walkable) {
        self.frame_speed = self.move_speed();
        self.step_line(dt, terrain);
        self.animated_tick(dt, terrain);
    }

    /// The part of the Animated tick that moves the entity: `SprintEnd`'s move event, its end.
    fn animated_tick(&mut self, dt: f32, terrain: &dyn Walkable) {
        if self.end_anim <= 0.0 {
            return;
        }
        if let Some(rate) = self.coast_due.take() {
            self.start_coast(rate, terrain);
        }
        self.end_anim = (self.end_anim - dt).max(0.0);
        if self.end_anim <= 0.0 && self.coasting {
            // `0x141da7dc0`: the animation changed, its move stops.
            self.line.kill();
            self.coasting = false;
            self.curve = None;
        }
    }

    /// `FUN_141ddc9d0`: advances the line and makes its steps.
    fn step_line(&mut self, dt: f32, terrain: &dyn Walkable) {
        if self.line.thr == 0.0 {
            return;
        }
        self.line.advance(dt, self.curve.as_mut());
        while self.line.wants_step() {
            let (cell, done) = self.line.step();
            if !Self::free(terrain, cell) {
                // Blocked: the cell is not applied; `FUN_141de5f50`, then the done delegates.
                self.line.kill();
                self.line_done(terrain);
                return;
            }
            self.grid = cell;
            if done {
                self.line_done(terrain);
                if self.line.thr == 0.0 {
                    self.line.acc = 0.0;
                    self.line.pending = 0.0;
                    return;
                }
                if self.line.pending > 0.0 {
                    self.line.advance(0.0, self.curve.as_mut());
                }
            }
        }
    }

    /// `0x141dccdb0`, the Pathfinding's handler of a finished line.
    fn line_done(&mut self, terrain: &dyn Walkable) {
        if self.coasting {
            self.coasting = false;
            self.curve = None;
            return;
        }
        if self.roll.is_some() || self.walk == Walk::None {
            return;
        }
        if let Walk::Target(goal) = self.walk
            && self.grid == goal
        {
            self.end_path();
            return;
        }
        if self.next < self.path.len() {
            let i = self.next;
            self.next += 1;
            self.step_along(i, terrain);
        } else {
            self.plan(terrain);
        }
    }

    /// `0x141dccac0(pf, 0)`.
    fn end_path(&mut self) {
        self.walk = Walk::None;
        self.path.clear();
        self.next = 0;
    }

    /// `FUN_141de5f50`.
    pub fn stop_line(&mut self) {
        self.line.kill();
    }

    /// `0x141ddb6f0`: the client's snap on a `0x64` whose cell is not its grid.
    pub fn snap(&mut self, cell: Cell) {
        self.grid = cell;
        self.line.kill();
        self.line.set_cur(cell);
    }

    // --- Pathfinding ---

    /// `0x141dcc6a0`: a search from the grid to the walk's goal, then the first waypoint.
    fn plan(&mut self, terrain: &dyn Walkable) {
        let goal = match self.walk {
            Walk::Axes => (
                self.grid.0 + floor_goal(self.axes.0),
                self.grid.1 + floor_goal(self.axes.1),
            ),
            Walk::Target(goal) => goal,
            Walk::None => return,
        };
        self.plans += 1;
        let goal = self.clamp_goal(goal);
        let moving = self.on_line().then(|| self.line.point());
        let params = Params {
            moving,
            ..Params::default()
        };
        let plan = clientpath::plan(terrain, self.grid, goal, self.level, &params);
        self.path = plan.waypoints().collect();
        if self.path.is_empty() {
            self.end_path();
            return;
        }
        self.next = 1;
        self.step_along(0, terrain);
    }

    fn clamp_goal(&self, goal: Cell) -> Cell {
        let (dx, dy) = ((goal.0 - self.grid.0) as i64, (goal.1 - self.grid.1) as i64);
        let d2 = dx * dx + dy * dy;
        if d2 < GOAL_LIMIT_SQ {
            return goal;
        }
        let k = GOAL_CLAMP / (d2 as f32).sqrt();
        (
            self.grid.0 + round(dx as f32 * k),
            self.grid.1 + round(dy as f32 * k),
        )
    }

    /// `Pathfinding_StepAlongPath`.
    fn step_along(&mut self, i: usize, terrain: &dyn Walkable) {
        let w = self.sprint_turn(self.path[i]);
        self.move_to(w, self.move_speed(), false, terrain);
    }

    /// `0x141dcc460`.
    fn sprint_turn(&self, w: Cell) -> Cell {
        if !self.is_sprinting {
            return w;
        }
        let (dx, dy) = (w.0 - self.grid.0, w.1 - self.grid.1);
        let (x, y) = (
            (dx as f32 + 0.5) * UNITS_PER_CELL,
            (dy as f32 + 0.5) * UNITS_PER_CELL,
        );
        let n = (x * x + y * y).sqrt();
        let d = (x / n, y / n);
        let f = (self.facing.sin(), -self.facing.cos());
        let c = d.0 * f.0 + d.1 * f.1;
        if c > 0.99 {
            return w;
        }
        let mut a = self.turn_rate * self.turn_factor * PI / 180.0;
        if c > -0.99 && c.acos() < a {
            return w;
        }
        if d.1 * f.0 - d.0 * f.1 < 0.0 {
            a = -a;
        }
        let r = (
            f.0 * a.cos() - f.1 * a.sin(),
            f.0 * a.sin() + f.1 * a.cos(),
        );
        (
            ((world(self.grid.0 as f32) + r.0 * SPRINT_TURN_AHEAD * UNITS_PER_CELL) * PER_UNIT).floor()
                as i32,
            ((world(self.grid.1 as f32) + r.1 * SPRINT_TURN_AHEAD * UNITS_PER_CELL) * PER_UNIT).floor()
                as i32,
        )
    }

    /// `Positioned_MoveToWaypoint` (`0x141de31a0`).
    fn move_to(&mut self, target: Cell, speed: f32, distance_mode: bool, terrain: &dyn Walkable) {
        if speed <= 0.0 {
            self.line.kill();
            return;
        }
        if target == self.grid {
            self.line_done_at_target(terrain);
            return;
        }
        self.facing = facing_of(
            (target.0 - self.grid.0) as f32,
            (target.1 - self.grid.1) as f32,
        );
        if self.roll.as_ref().is_some_and(|r| r.backward) {
            // Orientation mode 2 (`0x141de3afa`).
            self.facing = (self.facing - PI).rem_euclid(TAU);
        }
        self.line.setup(self.grid, target, speed, distance_mode);
    }

    /// Target == cell: the done delegates run, no line.
    fn line_done_at_target(&mut self, terrain: &dyn Walkable) {
        if self.roll.is_some() {
            return;
        }
        if self.next < self.path.len() {
            let i = self.next;
            self.next += 1;
            self.step_along(i, terrain);
        } else {
            self.end_path();
        }
    }

    // --- Actor ---

    /// The `0x64` vector (`0x1421497c0`, after its snap): each one is a plan.
    pub fn set_axes(&mut self, v: (i32, i32), terrain: &dyn Walkable) {
        if v == (0, 0) && self.axes == (0, 0) {
            return;
        }
        self.axes = v;
        if let Some(r) = &mut self.roll
            && v != (0, 0)
        {
            // The roll runs along the keys while one is held.
            r.heading = facing_of(floor_goal(v.0) as f32, floor_goal(v.1) as f32);
            r.keys = true;
        }
        self.start_wasd(terrain);
    }

    /// `Actor_StartWasdWalk` (`0x141e4fb40`).
    fn start_wasd(&mut self, terrain: &dyn Walkable) {
        if self.roll.is_some() {
            return;
        }
        if self.axes == (0, 0) {
            if self.walk == Walk::Axes {
                self.ending = 2;
            }
            return;
        }
        if self.end_anim > 0.0 {
            return;
        }
        self.ending = 0;
        self.walk = Walk::Axes;
        self.plan_now(terrain);
    }

    /// `FUN_141e42650`.
    fn plan_now(&mut self, terrain: &dyn Walkable) {
        self.plan_timer = PLAN_PERIOD;
        self.plan(terrain);
    }

    /// Click-to-move: a Move with a target.
    pub fn walk_to(&mut self, goal: Cell, terrain: &dyn Walkable) {
        self.walk = Walk::Target(goal);
        self.ending = 0;
        self.plan_now(terrain);
    }

    /// `0x141e42a00`.
    pub fn actor_tick(&mut self, dt: f32, terrain: &dyn Walkable) {
        if self.ending > 0 {
            self.ending -= 1;
            if self.ending == 0 {
                self.end_path();
                self.line.kill();
            }
        }
        if self.roll.is_some() {
            self.roll_tick(dt, terrain);
        } else if self.axes != (0, 0) {
            if self.walk == Walk::None {
                self.start_wasd(terrain);
            } else if self.walk == Walk::Axes && self.plan_timer <= 0.0 {
                self.plan_now(terrain);
            }
        } else if let Walk::Target(_) = self.walk
            && self.plan_timer <= 0.0
        {
            // Clicks plan only when their line ends: the timer is not used for them.
        }
        // `0x141b76da0`, a part of this tick: after the re-aim, before the timer.
        if self.sprint_on && !self.is_sprinting {
            self.end_sprint();
        }
        self.plan_timer = (self.plan_timer - dt).max(0.0);
    }

    /// The stat `is_sprinting` changes.
    pub fn set_sprinting(&mut self, on: bool) {
        self.is_sprinting = on;
        if on {
            self.sprint_on = true;
        }
    }

    /// The server's echo of the sprint key's release: without `must_sprint` it ends the sprint at
    /// once, so before the stats that follow it in the batch (sprint speed sets the coast's rate).
    pub fn sprint_key_released(&mut self) {
        if self.sprint_on && !self.must_sprint {
            self.end_sprint();
        }
    }

    /// `0x141b773a0`: ends Move, stops the entity, and with no action running plays `SprintEnd`.
    fn end_sprint(&mut self) {
        self.sprint_on = false;
        self.end_path();
        self.ending = 0;
        self.line.kill();
        if self.roll.is_some() {
            return;
        }
        if let Some(curve) = self.curves.as_ref().map(|c| &c.sprint_end) {
            // `0x141e56ae0`: movement speed in units over the animation's speed.
            let rate = self.frame_speed * UNITS_PER_CELL
                / (SPRINT_END_ANIM_SPEED * self.offset_scale);
            self.end_anim = curve.length / rate;
            self.coast_due = Some(rate);
        }
    }

    // --- Roll ---

    /// The `0x1c8` of a roll: `cell` is the message's start cell, `target` its target.
    pub fn start_roll(
        &mut self,
        cell: Cell,
        target: Cell,
        total: i32,
        dt: f32,
        terrain: &dyn Walkable,
    ) {
        if cell != self.grid {
            self.snap(cell);
        }
        self.line.kill();
        self.end_path();
        self.ending = 0;
        self.coasting = false;
        let playing_end_anim = self.end_anim;
        self.end_anim = 0.0;
        self.coast_due = None;
        // `DodgeRoll_InitHeading` (`0x141b8bda0`).
        let heading = if self.axes != (0, 0) {
            facing_of(floor_goal(self.axes.0) as f32, floor_goal(self.axes.1) as f32)
        } else if target != self.grid {
            facing_of(
                (target.0 - self.grid.0) as f32,
                (target.1 - self.grid.1) as f32,
            )
        } else {
            self.facing
        };
        // The action start turns to its target, so a roll against the cursor runs backwards.
        let faces = if target != self.grid {
            facing_of(
                (target.0 - self.grid.0) as f32,
                (target.1 - self.grid.1) as f32,
            )
        } else {
            self.facing
        };
        let off = (heading - faces).rem_euclid(TAU);
        // Not while `SprintEnd` plays (`LINE`: one roll each way; the reason is unread).
        let backward = playing_end_anim <= 0.0 && off.min(TAU - off) >= PI / 2.0;
        self.facing = heading;
        self.curve = self.curves.as_ref().map(|c| {
            CurveRun::load(if backward { c.roll_back.clone() } else { c.roll.clone() })
        });
        self.roll = Some(Roll {
            heading,
            total,
            elapsed: 0.0,
            steering: true,
            extend: None,
            keys: self.axes != (0, 0),
            backward,
            level: self.level,
        });
        // The action start: a plan from the cell and one step (the second plan is the frame's
        // Actor tick).
        self.roll_step(terrain);
        self.positioned_tick(dt, terrain);
    }

    /// A new target for the running roll (`0x1cc`, the cursor): it steers when no key is held.
    pub fn steer_roll(&mut self, target: Cell) {
        if target == self.grid {
            return;
        }
        let d = (
            (target.0 - self.grid.0) as f32,
            (target.1 - self.grid.1) as f32,
        );
        if let Some(r) = &mut self.roll
            && !r.keys
        {
            r.heading = facing_of(d.0, d.1);
        }
    }

    /// The roll's animation changes to its sprint variant: same time, new curve (`FUN_141b8cd50`).
    pub fn set_roll_sprinting(&mut self) {
        let Some(curves) = self.curves.clone() else { return };
        if let (Some(run), Some(r)) = (&mut self.curve, &self.roll) {
            let t = r.elapsed;
            *run = CurveRun::load(if r.backward {
                curves.roll_back_sprint.clone()
            } else {
                curves.roll_sprint.clone()
            });
            run.restart(t);
        }
    }

    /// The sprinting roll's lines run this far (see [`Roll::extend`]).
    pub fn extend_roll(&mut self, cells: f32) {
        if let Some(r) = &mut self.roll {
            r.extend = Some(cells);
        }
    }

    /// The roll action ends.
    pub fn end_roll(&mut self) {
        self.roll = None;
    }

    fn roll_left(&self) -> f32 {
        match (&self.curve, &self.roll) {
            (Some(c), Some(r)) => (1.0 - c.done()) * r.total as f32,
            _ => 0.0,
        }
    }

    /// `FUN_141b8c720`.
    fn roll_tick(&mut self, dt: f32, terrain: &dyn Walkable) {
        if let Some(r) = &mut self.roll {
            r.elapsed += dt;
        }
        self.roll_step(terrain);
    }

    /// `FUN_141b8c7f0`.
    fn roll_step(&mut self, terrain: &dyn Walkable) {
        let left = self.roll_left();
        let Some(r) = self.roll.clone() else { return };
        if left <= 1.0 || !r.steering {
            return;
        }
        let along = |h: f32, d: f32| (d * h.sin(), -d * h.cos());
        let base = if self.on_line() {
            self.line.point()
        } else {
            (self.grid.0 as f32, self.grid.1 as f32)
        };
        // `FUN_141b8d1f0`.
        let v = along(r.heading, left.max(ROLL_MIN_LOOKAHEAD));
        let far = (round(base.0 + v.0), round(base.1 + v.1));
        let moving = self.on_line().then(|| self.line.point());
        let plan = clientpath::plan(
            terrain,
            self.grid,
            far,
            r.level,
            &Params {
                moving,
                ..Params::default()
            },
        );
        let goal = plan.waypoints().next().unwrap_or(self.grid);
        // `FUN_141b8d000`: the point `left` away, towards the waypoint.
        let to = (goal.0 as f32 - base.0, goal.1 as f32 - base.1);
        let h = to.1.atan2(to.0) + PI / 2.0;
        let w = along(h, left);
        let target = (round(base.0 + w.0), round(base.1 + w.1));
        let (gx, gy) = (
            (goal.0 - self.grid.0) as f32,
            (goal.1 - self.grid.1) as f32,
        );
        let (tx, ty) = (
            (target.0 - self.grid.0) as f32,
            (target.1 - self.grid.1) as f32,
        );
        // `FUN_141b8d770`, `FUN_141dde3f0` (`LINE`: the restart also runs when no line follows).
        let Some(run) = &mut self.curve else { return };
        let time_left = run.curve.length - r.elapsed;
        if time_left <= 0.0 {
            return;
        }
        let speed = left / time_left;
        run.restart(r.elapsed);
        if (tx * tx + ty * ty).sqrt() < ROLL_STOP_DISTANCE {
            return;
        }
        // `FUN_141b8fe20`: a wall within 2 cells of the start: no move this frame.
        if Self::blocked_within(terrain, self.grid, target, 2) {
            return;
        }
        // `FUN_141b90060`: the line ends before the first wall on the way.
        let target = Self::last_free(terrain, self.grid, target);
        if let Some(ahead) = r.extend {
            // The optional length (`+0x44`, `+0x48`): a long line along the waypoint's direction
            // (`LINE`: not the target's, they differ once `left` is under the look-ahead); the
            // distance per frame stays that of the real target (`+0x280` = speed / its distance).
            let d = (
                (target.0 - self.grid.0) as f32,
                (target.1 - self.grid.1) as f32,
            );
            let dist = (d.0 * d.0 + d.1 * d.1).sqrt();
            let v = along(gy.atan2(gx) + PI / 2.0, ahead);
            let far = (self.grid.0 + round(v.0), self.grid.1 + round(v.1));
            self.move_to(far, speed, true, terrain);
            if dist > 0.0 {
                self.line.rate = speed / dist;
            }
        } else {
            self.move_to(target, speed, true, terrain);
        }
    }

    /// A Bresenham walk from `from` to `to` (`FUN_141b90060`): calls `each` per cell after the
    /// first, until it returns false.
    fn walk_cells(from: Cell, to: Cell, mut each: impl FnMut(Cell) -> bool) {
        let swap = (to.0 - from.0).abs() < (to.1 - from.1).abs();
        let (a0, b0, a1, b1) = if swap {
            (from.1, from.0, to.1, to.0)
        } else {
            (from.0, from.1, to.0, to.1)
        };
        let (da, db) = ((a1 - a0).abs(), (b1 - b0).abs());
        let (sa, sb) = (if a0 < a1 { 1 } else { -1 }, (b1 - b0).signum());
        let mut err = -(da / 2);
        let (mut a, mut b) = (a0, b0);
        while a != a1 + sa {
            let c = if swap { (b, a) } else { (a, b) };
            if !each(c) {
                return;
            }
            a += sa;
            err += db;
            if err >= 1 {
                err -= da;
                b += sb;
            }
        }
    }

    /// `FUN_141b8fe20`: a wall within `cells` cells of `from`.
    fn blocked_within(terrain: &dyn Walkable, from: Cell, to: Cell, cells: i32) -> bool {
        let mut left = cells;
        let mut blocked = false;
        Self::walk_cells(from, to, |c| {
            if !Self::free(terrain, c) {
                blocked = true;
                return false;
            }
            left -= 1;
            left > 0
        });
        blocked
    }

    /// The last free cell of the walk from `from` to `to` (`FUN_141b90060`).
    fn last_free(terrain: &dyn Walkable, from: Cell, to: Cell) -> Cell {
        let mut last = from;
        Self::walk_cells(from, to, |c| {
            if !Self::free(terrain, c) {
                return false;
            }
            last = c;
            true
        });
        last
    }

    // --- Coast ---

    /// `0x141f5f650`: the `SprintEnd` animation's root motion: one line from the grid along the
    /// facing, the curve's displacement x `offset_scale` long, over the animation's time at `rate`.
    /// The speed comes from the rounded target (`LINE`: T stays length / rate).
    fn start_coast(&mut self, rate: f32, terrain: &dyn Walkable) {
        let Some(curve) = self.curves.as_ref().map(|c| c.sprint_end.clone()) else { return };
        let units = (curve.value(curve.length) - curve.value(0.0)) * self.offset_scale;
        let cells = units / UNITS_PER_CELL;
        let f = (self.facing.sin(), -self.facing.cos());
        let d = (round(f.0 * cells), round(f.1 * cells));
        let target = (self.grid.0 + d.0, self.grid.1 + d.1);
        let time = curve.length / rate;
        let len = ((d.0 * d.0 + d.1 * d.1) as f32).sqrt();
        self.line.kill();
        self.curve = Some(CurveRun::load(curve));
        self.coasting = true;
        self.move_to(target, len / time, true, terrain);
    }

    /// Replaces the line with a state logged by the client (`LINE`), for the offline comparison.
    pub fn load_line(&mut self, l: LoggedLine) {
        let major_minor = |v: Cell| if l.swap { (v.1, v.0) } else { v };
        let d = if l.t > 0.0 { l.el / l.t } else { 0.0 };
        self.grid = major_minor(l.cur);
        let e = major_minor(l.e);
        if e != l.s {
            self.facing = facing_of((e.0 - l.s.0) as f32, (e.1 - l.s.1) as f32);
        }
        let distance_mode = self.line.distance_mode && self.line.running();
        let rate = self.line.rate;
        self.line = Line {
            s: l.s,
            p: if l.swap { (l.p.1, l.p.0) } else { l.p },
            q: major_minor(l.q),
            e: major_minor(l.e),
            flag: l.flag,
            swap: l.swap,
            sign: l.sign,
            slope: l.slope,
            end_major: l.e.0,
            major: l.cur.0,
            minor: l.cur.1,
            minor_acc: l.minacc,
            distance_mode,
            len: l.len,
            t: l.t,
            inv_t: 1.0 / l.t,
            rate: if distance_mode { rate } else { 0.0 },
            elapsed: l.el,
            distance: d * l.len,
            thr: l.thr,
            acc: l.acc,
            pending: l.pend,
            step_time: l.t / (l.e.0 as f32 - l.p.0).abs(),
            pending_scale: 1.0,
        };
    }

    /// The client has no line (`LINE` not logged): the entity stands on `cell`.
    pub fn load_no_line(&mut self, cell: Cell) {
        self.grid = cell;
        self.line = Line {
            pending: self.line.pending,
            ..Line::default()
        };
    }
}
