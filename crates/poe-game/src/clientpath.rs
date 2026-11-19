//! A port of the client's grid pathfinder (`FUN_141edeb10`; the algorithm and the
//! addresses are in `docs/re/pathfinding.md`), for static terrain.
//!
//! The client plans every walk, WASD step, roll frame and sprint frame with this search, so a
//! server that wants the same positions near walls has to plan the same way. It is a jump point
//! search with bounded jumps over the 4-bit cell grid, followed by a line-of-sight pass that
//! thins the path:
//!
//! - The search window is 272 x 272 cells around the start (the goal is pulled in to 126 cells by
//!   the caller). Cells outside an ellipse around start and goal (focal sum `L`, see
//!   [`plan`]) count as walls.
//! - A cell is blocked when its grid value (layer 0, value 5 meaning "far from walls") is at most
//!   the entity's clearance `level`: 0 normally, 4 while it sprints after a roll.
//! - Jumps along a direction are cut after `3 + 5 * alignment + 0.2 * excess` cells (at most
//!   `cap`, 5 to 22), so the open list holds nodes a few cells apart.
//! - The node with the lowest remaining distance `h` wins when the goal cannot be reached, and
//!   the path is rebuilt backwards through the nodes keeping only the cells that break
//!   line of sight, then thinned once more in windows of five.
//!
//! The line-of-sight tests that end at the start run from the entity's exact point on its line
//! while it moves along one ([`Params::moving`]).
//!
//! A start inside a blocking object's ring runs the client's general A* instead ([`plan`]).
//! Not ported: dynamic obstacles (other entities), and the A* for a start whose terrain is
//! blocked at the entity's level (the search runs again at level 0 instead, which agreed with
//! the client).
//!
//! The heap reproduces MSVC's `std::push_heap`/`pop_heap`, because equal costs are common on a
//! grid and the client's tie order decides which of several equally good paths it takes.

// The client's constants are rounded decimals (0.7071, 1.4142), not the exact values.
#![allow(clippy::approx_constant)]

use std::cell::RefCell;

use crate::movement::Walkable;

pub type Cell = (i32, i32);

/// Half the side of the search window (the window is `2 * HALF` cells square).
const HALF: i32 = 0x88;
const SIDE: usize = 0x110;
/// Directions as the client numbers them: the offset a node's `dir` points to its parent by.
const DXY: [Cell; 8] = [
    (0, 1),
    (1, 0),
    (0, -1),
    (-1, 0),
    (1, 1),
    (1, -1),
    (-1, -1),
    (-1, 1),
];
/// Opposite direction.
const REV: [usize; 8] = [2, 3, 0, 1, 6, 7, 4, 5];
/// Unit vectors of the directions (the client's float table; the diagonals are 0.7071).
const UNIT: [(f32, f32); 8] = [
    (0.0, 1.0),
    (1.0, 0.0),
    (0.0, -1.0),
    (-1.0, 0.0),
    (0.7071, 0.7071),
    (0.7071, -0.7071),
    (-0.7071, -0.7071),
    (-0.7071, 0.7071),
];
/// Neighbour codes checked for forced neighbours, per direction.
const ROW: [[usize; 4]; 8] = [
    [1, 3, 5, 6],
    [0, 2, 7, 6],
    [1, 3, 4, 7],
    [0, 2, 4, 5],
    [0, 1, 5, 7],
    [2, 1, 6, 4],
    [2, 3, 5, 7],
    [0, 3, 6, 4],
];

const SQRT2: f32 = 1.4142;
/// Node flag bits: low nibble = direction to the parent (8 = the start), `CLOSED` once
/// classified or expanded, `BLOCKED` with it for a wall, `SEEN` for a cell a jump passed.
const CLOSED: u8 = 0x10;
const BLOCKED: u8 = 0x20;
const SEEN: u8 = 0x80;

/// What the planner needs from the search call.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Params {
    /// Lowest and highest wanted distance to the goal (0, 0: the goal cell itself).
    pub min_dist: u32,
    pub max_dist: u32,
    /// Stop this many cells from the goal (-1: none).
    pub radius: i8,
    /// The entity's exact position on the line it is moving along, in cell coordinates with
    /// cell centres on whole numbers (position - 0.5), when it moves along a line whose current
    /// cell is the start cell (`FUN_141b95df0`). The line-of-sight tests that end at the start
    /// then run from this point (`FUN_141eead80`) instead of from the start cell.
    pub moving: Option<(f32, f32)>,
    /// The entity's size level for the object rings (`local_658`, `FUN_141dd53c0`: the larger of
    /// `object_size` and `secondary_object_size`, 4 for a player also when walking; the terrain
    /// test uses `object_size` alone). `None`: the terrain level.
    pub size_level: Option<i32>,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            min_dist: 0,
            max_dist: 0,
            radius: -1,
            moving: None,
            size_level: None,
        }
    }
}

/// A blocking object the search walls off: an entity on layer 0 of the terrain's entity buckets
/// (`0x141d11e40`: `.ot` `Positioned { blocking = true }`, not `static`, not `trigger_blocking`)
/// that passes the search's filter (`0x141ee0180`: size not -1, not `non_blocking_but_hittable`)
/// and cannot be pushed (`0x141df03c0`; objects have no push priority).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Obstacle {
    pub cell: Cell,
    /// The larger of `object_size` and `secondary_object_size` (`Positioned +0x1f8`, `+0x1f9`).
    pub size: i32,
}

/// Entity buckets are 23 cells square (`0x17`).
const BUCKET: i32 = 0x17;

/// Result of [`plan`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    /// Waypoints from the end of the path back to the start, the last entry being the start
    /// cell itself (the client's order); empty when there is nothing to walk to.
    pub path: Vec<Cell>,
    /// The goal cell itself was reached.
    pub reached: bool,
}

impl Plan {
    /// The cells to walk to, in walking order (without the start).
    pub fn waypoints(&self) -> impl Iterator<Item = Cell> + '_ {
        self.path.iter().rev().skip(1).copied()
    }
}

#[derive(Clone, Copy, Default)]
struct Node {
    stamp: u16,
    flags: u8,
    g: f32,
    h: f32,
    f: f32,
}

#[derive(Default)]
struct Scratch {
    nodes: Vec<Node>,
    stamp: u16,
    heap: Vec<(u32, f32)>,
}

thread_local! {
    static SCRATCH: RefCell<Scratch> = RefCell::new(Scratch::default());
}

/// MSVC `std::push_heap` with a "greater f" comparator (a min-heap on f).
fn heap_push(heap: &mut Vec<(u32, f32)>, key: u32, f: f32) {
    heap.push((key, f));
    let mut hole = heap.len() - 1;
    let val = heap[hole];
    while hole > 0 {
        let parent = (hole - 1) >> 1;
        if heap[parent].1 <= val.1 {
            break;
        }
        heap[hole] = heap[parent];
        hole = parent;
    }
    heap[hole] = val;
}

/// MSVC `std::pop_heap` followed by `pop_back`: returns the old top.
fn heap_pop(heap: &mut Vec<(u32, f32)>) -> (u32, f32) {
    let n = heap.len();
    let top = heap[0];
    if n >= 2 {
        let val = heap[n - 1];
        heap[n - 1] = top;
        let bottom = n - 1;
        let mut hole = 0usize;
        let mut idx = 0usize;
        let max_non_leaf = (bottom as isize - 1) >> 1;
        while (idx as isize) < max_non_leaf {
            idx = 2 * idx + 2;
            if heap[idx].1 > heap[idx - 1].1 {
                idx -= 1;
            }
            heap[hole] = heap[idx];
            hole = idx;
        }
        if idx as isize == max_non_leaf && bottom.is_multiple_of(2) {
            heap[hole] = heap[bottom - 1];
            hole = bottom - 1;
        }
        while hole > 0 {
            let parent = (hole - 1) >> 1;
            if heap[parent].1 <= val.1 {
                break;
            }
            heap[hole] = heap[parent];
            hole = parent;
        }
        heap[hole] = val;
    }
    heap.pop();
    top
}

/// The client's `sqrt(float(x*x + y*y))` of an integer vector.
fn length(x: i32, y: i32) -> f32 {
    (x.wrapping_mul(x).wrapping_add(y.wrapping_mul(y)) as u32 as f32).sqrt()
}

struct Search<'a> {
    terrain: &'a dyn Walkable,
    bounds: Option<(i32, i32)>,
    scratch: &'a mut Scratch,
    start: Cell,
    goal: Cell,
    origin: Cell,
    level: i32,
    params: Params,
    /// Goal minus start.
    a: Cell,
    /// `(max_dist + radius)^2`: cells this close to the goal are tested for being the goal.
    goal2: u32,
    /// `max_dist + radius` as float.
    max_ext: f32,
    h_start: f32,
    cap: u32,
    ellipse: f32,
    mid: Cell,
    mid_r2: u32,
}

impl Search<'_> {
    fn index(&self, c: Cell) -> Option<usize> {
        let (ux, uy) = (
            c.0.wrapping_sub(self.origin.0) as u32,
            c.1.wrapping_sub(self.origin.1) as u32,
        );
        (ux < SIDE as u32 && uy < SIDE as u32).then(|| ux as usize * SIDE + uy as usize)
    }

    fn cell_of(&self, idx: usize) -> Cell {
        (
            self.origin.0 + (idx / SIDE) as i32,
            self.origin.1 + (idx % SIDE) as i32,
        )
    }

    /// The node record of an index, if the current search has touched it.
    fn node(&self, idx: usize) -> Option<&Node> {
        let n = &self.scratch.nodes[idx];
        (n.stamp == self.scratch.stamp).then_some(n)
    }

    /// The node record, created (zeroed) when it is new in this search.
    fn node_mut(&mut self, idx: usize) -> &mut Node {
        let stamp = self.scratch.stamp;
        let n = &mut self.scratch.nodes[idx];
        if n.stamp != stamp {
            *n = Node {
                stamp,
                ..Node::default()
            };
        }
        n
    }

    fn nibble(&self, c: Cell) -> i32 {
        match self.terrain.nibble(c.0, c.1) {
            5 => 255,
            v => v as i32,
        }
    }

    fn blocked_at(&self, c: Cell) -> bool {
        self.level != -1 && self.level >= self.nibble(c)
    }

    fn in_terrain(&self, c: Cell) -> bool {
        match self.bounds {
            Some((w, h)) => c.0 >= 0 && c.1 >= 0 && c.0 < w && c.1 < h,
            None => true,
        }
    }

    /// Walls off the blocking objects in the entity buckets around the start (`0x141ee1020`
    /// collects them, `0x141ee0930` marks them): buckets from the start's bucket minus
    /// `m / 23 + 1` to plus `m / 23 + 1`, `m = level - 1 + terrain +0x138` (the largest blocking
    /// size + 1, `0x141d1ccf0`). Each object closes a ring of cells as walls in the window:
    /// radius `R = level + size`, from `R - 2` (1 when `R < 3`, plus its own cell) to `R`.
    fn wall_off_obstacles(&mut self, obstacles: &[Obstacle], level: i32) {
        if obstacles.is_empty() {
            return;
        }
        let largest = obstacles.iter().map(|o| o.size.max(0) + 1).max().unwrap_or(0);
        let m = level - 1 + largest;
        let span = m / BUCKET + 1;
        let (bx, by) = (self.start.0 / BUCKET, self.start.1 / BUCKET);
        let (lo, hi) = ((bx - span, by - span), (bx + span, by + span));
        for o in obstacles {
            let b = (o.cell.0 / BUCKET, o.cell.1 / BUCKET);
            if b.0 < lo.0.max(0) || b.1 < lo.1.max(0) || b.0 > hi.0 || b.1 > hi.1 {
                continue;
            }
            self.wall_off_ring(o.cell, (level + o.size.max(0)) as u32);
        }
    }

    /// `0x141ee0930`'s marks for one object of ring radius `r`.
    fn wall_off_ring(&mut self, at: Cell, r: u32) {
        let ux = (at.0 - self.start.0 + HALF) as u32;
        let uy = (at.1 - self.start.1 + HALF) as u32;
        let mark = |s: &mut Self, x: u32, y: u32| {
            if x < SIDE as u32 && y < SIDE as u32 {
                s.node_mut(x as usize * SIDE + y as usize).flags |= CLOSED | BLOCKED;
            }
        };
        let inner = if r < 3 { 1 } else { r - 2 };
        if r < 3 {
            mark(self, ux, uy);
        }
        for k in inner..=r {
            if ux < SIDE as u32 {
                mark(self, ux, uy.wrapping_sub(k));
                mark(self, ux, uy.wrapping_add(k));
            }
        }
        if uy < SIDE as u32 {
            for k in inner..=r {
                mark(self, ux.wrapping_sub(k), uy);
                mark(self, ux.wrapping_add(k), uy);
            }
        }
        for dy in 1..=r {
            for dx in 1..=r {
                let d2 = dx * dx + dy * dy;
                if inner * inner <= d2 && d2 <= r * r {
                    mark(self, ux.wrapping_sub(dx), uy.wrapping_sub(dy));
                    mark(self, ux.wrapping_add(dx), uy.wrapping_sub(dy));
                    mark(self, ux.wrapping_sub(dx), uy.wrapping_add(dy));
                    mark(self, ux.wrapping_add(dx), uy.wrapping_add(dy));
                }
            }
        }
    }

    /// 0 free, 1 wall, 2 outside the terrain, the window's ellipse, or already expanded.
    fn classify(&mut self, c: Cell) -> u8 {
        let idx = self.index(c);
        if let Some(i) = idx
            && let Some(n) = self.node(i)
            && n.flags & CLOSED != 0
        {
            return if n.flags & BLOCKED != 0 { 1 } else { 2 };
        }
        let mark = |s: &mut Self, bits: u8| {
            if let Some(i) = idx {
                s.node_mut(i).flags |= bits;
            }
        };
        if !self.in_terrain(c) {
            mark(self, CLOSED);
            return 2;
        }
        if self.blocked_at(c) {
            mark(self, CLOSED | BLOCKED);
            return 1;
        }
        let d2 = (c.0 - self.mid.0).wrapping_mul(c.0 - self.mid.0)
            + (c.1 - self.mid.1).wrapping_mul(c.1 - self.mid.1);
        if d2 as u32 >= self.mid_r2 {
            let sum = length(c.0 - self.start.0, c.1 - self.start.1)
                + length(c.0 - self.goal.0, c.1 - self.goal.1);
            if self.ellipse <= sum {
                mark(self, CLOSED);
                return 2;
            }
        }
        0
    }

    /// Whether the neighbour of `pos` in direction code `code` is a wall.
    fn is_blocked(&mut self, pos: Cell, code: usize) -> bool {
        let c = (pos.0 + DXY[code].0, pos.1 + DXY[code].1);
        let Some(i) = self.index(c) else {
            return false;
        };
        let unclassified = self.node(i).is_none_or(|n| n.flags & CLOSED != 0);
        if unclassified {
            let r = self.classify(c);
            if r != 0 {
                return r == 1;
            }
            let n = self.node_mut(i);
            n.flags = (n.flags & 0xf0) | SEEN;
            n.g = f32::INFINITY;
        }
        false
    }

    /// Remaining-distance estimate of a cell.
    fn hcell(&self, c: Cell) -> f32 {
        let b = (c.0 - self.goal.0, c.1 - self.goal.1);
        let ln = length(b.0, b.1);
        let mut d = ln;
        if self.params.radius >= 0 {
            d = (ln - self.params.radius as f32).max(0.0);
        }
        let cross = self
            .a
            .1
            .wrapping_mul(b.0)
            .wrapping_sub(b.1.wrapping_mul(self.a.0))
            .wrapping_abs();
        let c = cross as f32 * 0.001f32;
        let (maxd, mind) = (self.params.max_dist as f32, self.params.min_dist as f32);
        if maxd < d {
            return (d - maxd) + c;
        }
        if mind > d {
            return (mind - d) + c;
        }
        0.0
    }

    fn jump_limit(&self, pos: Cell, d: usize, g: f32, h: f32, diagonal: bool) -> u32 {
        let v = ((pos.0 - self.goal.0) as f32, (pos.1 - self.goal.1) as f32);
        let dot = UNIT[d].1 * v.1 + UNIT[d].0 * v.0;
        let q = (dot / (self.max_ext + h)).max(0.0);
        let mut x = q * 5.0f32 + 3.0f32;
        x += ((g + h) - self.h_start) * 0.2f32;
        if diagonal {
            x *= 0.7071f32;
        }
        let n = x as i64;
        if n < 0 || n > u32::MAX as i64 {
            return self.cap;
        }
        (n as u32).min(self.cap)
    }

    fn push(&mut self, idx: usize) {
        let f = self.scratch.nodes[idx].f;
        heap_push(&mut self.scratch.heap, idx as u32, f);
    }

    /// Writes a finished node (g, h, f, direction) and queues it.
    fn finish(&mut self, idx: usize, g: f32, h: f32, d: usize) {
        let n = self.node_mut(idx);
        n.h = h;
        n.g = g;
        n.f = h + g;
        n.flags = (n.flags & 0x70) | d as u8;
        self.push(idx);
    }

    fn jump_straight(&mut self, pos: Cell, d: usize, g: f32, h: f32) {
        let limit = self.jump_limit(pos, d, g, h, false);
        let step = (-DXY[d].0, -DXY[d].1);
        let row = ROW[d];
        let (mut flag_a, mut flag_b) = (false, false);
        let (mut x, mut y) = pos;
        let mut k = 1u32;
        let newg = loop {
            x += step.0;
            y += step.1;
            let c = (x, y);
            let Some(i) = self.index(c) else {
                return;
            };
            if self.node(i).is_none_or(|n| n.flags & CLOSED != 0) && self.classify(c) != 0 {
                return;
            }
            let newg = k as f32 + g;
            if let Some(n) = self.node(i)
                && n.flags & CLOSED == 0
                && n.g <= newg
            {
                return;
            }
            if k == limit {
                break newg;
            }
            let mut forced = false;
            if flag_a || self.is_blocked(c, row[0]) {
                if !self.is_blocked(c, row[2]) {
                    forced = true;
                } else {
                    flag_a = true;
                }
            }
            if !forced && (flag_b || self.is_blocked(c, row[1])) {
                if !self.is_blocked(c, row[3]) {
                    forced = true;
                } else {
                    flag_b = true;
                }
            }
            if forced {
                break newg;
            }
            let (dx, dy) = (x - self.goal.0, y - self.goal.1);
            if (dx.wrapping_mul(dx).wrapping_add(dy.wrapping_mul(dy))) as u32 <= self.goal2
                && self.hcell(c) == 0.0
            {
                self.finish(i, newg, 0.0, d);
                return;
            }
            let n = self.node_mut(i);
            n.flags = (n.flags & 0xf0) | d as u8 | SEEN;
            n.g = newg;
            k += 1;
        };
        let c = (x, y);
        let h = self.hcell(c);
        if let Some(i) = self.index(c) {
            self.finish(i, newg, h, d);
        }
    }

    fn jump_diagonal(&mut self, pos: Cell, d: usize, g: f32, h: f32) {
        let limit = self.jump_limit(pos, d, g, h, true);
        let step = (-DXY[d].0, -DXY[d].1);
        let row = ROW[d];
        let (mut x, mut y) = pos;
        let mut k = 1u32;
        loop {
            x += step.0;
            y += step.1;
            let c = (x, y);
            let Some(i) = self.index(c) else {
                return;
            };
            if self.node(i).is_none_or(|n| n.flags & CLOSED != 0) && self.classify(c) != 0 {
                return;
            }
            let newg = k as f32 * SQRT2 + g;
            if let Some(n) = self.node(i)
                && n.flags & CLOSED == 0
                && n.g <= newg
            {
                return;
            }
            let hh = self.hcell(c);
            let create = k == limit
                || hh == 0.0
                || (self.is_blocked(c, row[0]) && !self.is_blocked(c, row[2]))
                || (self.is_blocked(c, row[1]) && !self.is_blocked(c, row[3]));
            if create {
                self.finish(i, newg, hh, d);
                return;
            }
            self.jump_straight(c, row[0], newg, hh);
            self.jump_straight(c, row[1], newg, hh);
            let n = self.node_mut(i);
            n.flags = (n.flags & 0xf0) | d as u8 | SEEN;
            n.g = newg;
            k += 1;
        }
    }

    /// The client's cell-by-cell line check between two cells (blocked cells and cells outside
    /// the window end it).
    fn los(&self, a: Cell, b: Cell) -> bool {
        let (dx, dy) = ((b.0 - a.0).abs(), (b.1 - a.1).abs());
        let steep = dx < dy;
        let (m1, n1, m2, n2) = if steep {
            (a.1, a.0, b.1, b.0)
        } else {
            (a.0, a.1, b.0, b.1)
        };
        let (dm_major, dm_minor) = ((m2 - m1).abs(), (n2 - n1).abs());
        let step_minor = (n2 - n1).signum();
        let step_major = if m1 < m2 { 1 } else { -1 };
        let mut err = -(dm_major / 2);
        let (mut m, mut n) = (m1, n1);
        while m != m2 + step_major {
            let c = if steep { (n, m) } else { (m, n) };
            if !self.sight_cell(c) {
                return false;
            }
            err += dm_minor;
            m += step_major;
            if err >= 1 {
                n += step_minor;
                err -= dm_major;
            }
        }
        true
    }

    /// Whether line of sight passes a cell: not a wall at the entity's level, inside the window
    /// and not a node the search closed as a wall.
    fn sight_cell(&self, c: Cell) -> bool {
        if self.blocked_at(c) {
            return false;
        }
        let Some(i) = self.index(c) else {
            return false;
        };
        !self
            .node(i)
            .is_some_and(|nd| nd.flags & (CLOSED | BLOCKED) == CLOSED | BLOCKED)
    }

    /// Line of sight from the start to `d`, or from the moving entity's exact point `p` when
    /// it moves along a line ([`Params::moving`]; `FUN_141ed8df0` + `FUN_141eead80`).
    ///
    /// The client projects the start cell onto the line `p -> d` (`q`, rounded), walks from the
    /// start cell to `q` one step on each axis at a time, then walks the float line from `p` to
    /// `d` cell by cell along its major axis, from `q`'s major coordinate on.
    fn los_from_start(&self, d: Cell) -> bool {
        let Some(p) = self.params.moving else {
            return self.los(self.start, d);
        };
        let round = |v: f32| (v + 0.5).floor() as i32;
        let v = (d.0 as f32 - p.0, d.1 as f32 - p.1);
        let w = (self.start.0 as f32 - p.0, self.start.1 as f32 - p.1);
        let t = (v.0 * w.0 + v.1 * w.1) / (v.0 * v.0 + v.1 * v.1);
        let q = (round(v.0 * t + p.0), round(v.1 * t + p.1));
        let mut c = self.start;
        while c != q {
            if !self.sight_cell(c) {
                return false;
            }
            c.0 += (q.0 - c.0).signum();
            c.1 += (q.1 - c.1).signum();
        }
        // The float line, in (major, minor) order.
        let steep = (d.1 - q.1).abs() > (d.0 - q.0).abs();
        let (mut major, mut minor, end, end_minor, pm, pn) = if steep {
            (q.1, q.0, d.1, d.0, p.1, p.0)
        } else {
            (q.0, q.1, d.0, d.1, p.0, p.1)
        };
        let step = if major < end { 1 } else { -1 };
        let span = (end as f32 - pm).abs();
        let slope = (end_minor as f32 - pn) * (1.0 / span);
        let behind = ((end - major).abs() as f32) - span;
        let bias = if slope > 0.0 { 0.49 } else { 0.5 };
        let mut acc = pn - slope * behind + bias;
        while major != end + step {
            let c = if steep {
                (minor, major)
            } else {
                (major, minor)
            };
            if !self.sight_cell(c) {
                return false;
            }
            acc += slope;
            minor = acc.floor() as i32;
            major += step;
        }
        true
    }

    fn run(&mut self) -> Plan {
        let start = self.start;
        let si = self.index(start).expect("start in window");
        {
            let hs = self.h_start;
            let n = self.node_mut(si);
            n.g = 0.0;
            n.h = hs;
            n.f = hs;
            n.flags = (n.flags & 0x70) | 8;
        }
        self.push(si);
        let (mut best, mut best_h) = (start, self.h_start);
        let mut reached = None;
        while !self.scratch.heap.is_empty() {
            let (key, f) = heap_pop(&mut self.scratch.heap);
            let i = key as usize;
            let Some(&n) = self.node(i) else { continue };
            if f != n.f {
                continue;
            }
            let pos = self.cell_of(i);
            if n.h < best_h {
                best = pos;
                best_h = n.h;
            }
            if n.h == 0.0 {
                reached = Some(pos);
                break;
            }
            self.node_mut(i).flags |= CLOSED;
            let p = (n.flags & 0xf) as usize;
            let (g, h) = (n.g, n.h);
            if p == 8 {
                for u in 0..8 {
                    let c2 = (pos.0 + DXY[u].0, pos.1 + DXY[u].1);
                    let Some(j) = self.index(c2) else { continue };
                    if self.classify(c2) == 0 {
                        let step = if u < 4 { 1.0 } else { SQRT2 };
                        let hh = self.hcell(c2);
                        let nn = self.node_mut(j);
                        nn.g = step;
                        nn.h = hh;
                        nn.f = step + hh;
                        nn.flags = (nn.flags & 0x70) | REV[u] as u8;
                        self.push(j);
                    }
                }
                continue;
            }
            let row = ROW[p];
            let straight = if p > 3 {
                self.jump_diagonal(pos, p, g, h);
                self.jump_straight(pos, row[0], g, h);
                row[1]
            } else {
                p
            };
            self.jump_straight(pos, straight, g, h);
            for (side, behind) in [(row[0], row[2]), (row[1], row[3])] {
                let c2 = (pos.0 + DXY[side].0, pos.1 + DXY[side].1);
                let Some(j) = self.index(c2) else { continue };
                if self.node(j).is_none_or(|nn| nn.flags & CLOSED != 0) {
                    match self.classify(c2) {
                        0 => {
                            let nn = self.node_mut(j);
                            nn.g = f32::INFINITY;
                            nn.flags = (nn.flags & 0xf0) | REV[side] as u8 | SEEN;
                        }
                        1 => self.jump_diagonal(pos, REV[behind], g, h),
                        _ => {}
                    }
                }
            }
        }
        let end = reached.unwrap_or(best);
        if end == start {
            return Plan {
                path: Vec::new(),
                reached: reached.is_some(),
            };
        }
        // Rebuild backwards, keeping the cells where line of sight to the last kept one ends.
        let parent = |s: &Self, c: Cell| -> Cell {
            let d = s
                .index(c)
                .and_then(|i| s.node(i))
                .map_or(8, |n| (n.flags & 0xf) as usize)
                .min(7);
            (c.0 + DXY[d].0, c.1 + DXY[d].1)
        };
        let mut path = vec![end];
        let mut kept = end;
        let mut c = parent(self, end);
        while c != start {
            let next = parent(self, c);
            let sight = if next == start {
                self.los_from_start(kept)
            } else {
                self.los(next, kept)
            };
            if !sight {
                path.push(c);
                kept = c;
            }
            c = next;
            if path.len() >= 0x88 {
                return Plan::default();
            }
        }
        path.push(start);
        if path.len() > 2 {
            let mut i = 0;
            while i < path.len() {
                let last = path.len() - 1;
                let mut j = (i + 5).min(last);
                while j > i {
                    let sight = if j == last {
                        self.los_from_start(path[i])
                    } else {
                        self.los(path[j], path[i])
                    };
                    if sight {
                        path.drain(i + 1..j);
                        break;
                    }
                    j -= 1;
                }
                i += 1;
            }
        }
        Plan {
            path,
            reached: reached.is_some(),
        }
    }
}

/// The general A* (`0x141edc7d0`) the search falls back to: its neighbour order per parent
/// code (`DAT_142f1f670`, the first `ASTAR_COUNT[code]` entries; code 8 is the start).
const ASTAR_ORDER: [[usize; 8]; 9] = [
    [6, 2, 5, 0, 0, 0, 0, 0],
    [6, 3, 7, 0, 0, 0, 0, 0],
    [7, 0, 4, 0, 0, 0, 0, 0],
    [5, 1, 4, 0, 0, 0, 0, 0],
    [6, 2, 5, 3, 7, 0, 0, 0],
    [6, 3, 7, 0, 4, 0, 0, 0],
    [5, 1, 7, 0, 4, 0, 0, 0],
    [6, 2, 5, 1, 4, 0, 0, 0],
    [6, 2, 5, 3, 1, 7, 0, 4],
];
/// `DAT_142f1f6b8`: how many neighbours a node with each parent code expands.
const ASTAR_COUNT: [usize; 9] = [3, 3, 3, 3, 5, 5, 5, 5, 8];
/// Step cost into a cell inside a blocking object's ring (from a node that is inside one).
const ASTAR_ENTITY_STEP: f32 = 21.0;

impl Search<'_> {
    /// The entity cell test (`0x141ede210` / `0x141ede680`, docs/re/server-code.md section 3)
    /// for objects, which cannot be pushed: inside a ring of radius `ring_level + size`.
    fn entity_blocked(&self, c: Cell, ring_level: i32) -> bool {
        self.terrain.obstacles().iter().any(|o| {
            let (dx, dy) = (o.cell.0 - c.0, o.cell.1 - c.1);
            let r = (ring_level + 1 + o.size.max(0)).max(1) - 1;
            ((dx * dx + dy * dy) as u32) < (r * r) as u32
        })
    }

    /// Whether the start cell is inside a blocking object's ring: the client's search then
    /// runs the general A* (`0x141ee008a` calls `0x141edc7d0`).
    fn start_in_entity(&self) -> bool {
        self.entity_blocked(self.start, self.params.size_level.unwrap_or(self.level))
    }

    /// The cells of the client's integer line from `a` to `b`, both included
    /// (`0x141d1d070` and the A*'s smoothing walk them the same way).
    fn line_any(a: Cell, b: Cell, mut hit: impl FnMut(Cell) -> bool) -> bool {
        let (dx, dy) = ((b.0 - a.0).abs(), (b.1 - a.1).abs());
        let steep = dx < dy;
        let (m1, n1, m2, n2) = if steep { (a.1, a.0, b.1, b.0) } else { (a.0, a.1, b.0, b.1) };
        let (dmaj, dmin) = ((m2 - m1).abs(), (n2 - n1).abs());
        let step_minor = (n2 - n1).signum();
        let step_major = if m1 < m2 { 1 } else { -1 };
        let mut err = -(dmaj / 2);
        let (mut m, mut n) = (m1, n1);
        while m != m2 + step_major {
            if hit(if steep { (n, m) } else { (m, n) }) {
                return true;
            }
            err += dmin;
            if err > 0 {
                n += step_minor;
                err -= dmaj;
            }
            m += step_major;
        }
        false
    }

    /// `0x141d1d070`: a cell on the line from `a` to `b` has a free size below `level + 1`.
    fn walk_blocked(&self, a: Cell, b: Cell) -> bool {
        let need = self.level + 1;
        need != 0 && Self::line_any(a, b, |c| self.nibble(c) < need)
    }

    /// The general A* `0x141edc7d0` for a start inside a blocking object's ring
    /// (docs/re/server-code.md section 2), for a player: unlimited node budget, the ellipse
    /// bound, no coarse retry. A cell inside a ring can be entered only from a node inside one,
    /// at cost 21, so the path leaves the ring by the fewest ring cells.
    fn astar(&mut self) -> Plan {
        let start = self.start;
        let ring_level = self.params.size_level.unwrap_or(self.level);
        // `max(min(2 d, 136), 20)` (`0x1401528a0` / `0x1401528b0`; 50 for a player's minion).
        let bound = (length(self.a.0, self.a.1) * 2.0).min(136.0).max(20.0);
        let terrain_ok = |s: &Self, c: Cell| s.level == -1 || s.nibble(c) >= s.level + 1;
        let start_blocked = !terrain_ok(self, start) || self.entity_blocked(start, ring_level);
        let si = self.index(start).expect("start in window");
        let h0 = self.hcell(start);
        {
            let n = self.node_mut(si);
            n.g = 0.0;
            n.h = h0;
            n.f = h0;
            n.flags = (n.flags & 0x70) | 8 | if start_blocked { BLOCKED } else { 0 };
        }
        self.push(si);
        let (mut best, mut best_h) = (start, h0);
        let mut reached = false;
        while !self.scratch.heap.is_empty() {
            let (key, f) = heap_pop(&mut self.scratch.heap);
            let i = key as usize;
            let Some(&n) = self.node(i) else { continue };
            if f != n.f {
                continue;
            }
            let pos = self.cell_of(i);
            if n.h < best_h {
                best = pos;
                best_h = n.h;
            }
            // `h == 0`: reached (the client's extra line test `0x141d1d4f0` for a goal band
            // is not ported; a point goal has `h == 0` only on the goal cell).
            if n.h == 0.0 {
                best = pos;
                reached = true;
                break;
            }
            let flags = {
                let nm = self.node_mut(i);
                nm.flags |= CLOSED;
                nm.flags
            };
            let code = (flags & 0xf) as usize;
            let inside = flags & BLOCKED != 0;
            for &d in &ASTAR_ORDER[code.min(8)][..ASTAR_COUNT[code.min(8)]] {
                let c2 = (pos.0 + DXY[d].0, pos.1 + DXY[d].1);
                let Some(j) = self.index(c2) else { continue };
                let base = if d < 4 { 1.0 } else { SQRT2 };
                match self.node(j).copied() {
                    Some(nb) if nb.flags & CLOSED != 0 => {}
                    Some(nb) => {
                        let g = if nb.flags & BLOCKED != 0 { ASTAR_ENTITY_STEP } else { base } + n.g;
                        if g < nb.g {
                            let h = self.hcell(c2);
                            let nm = self.node_mut(j);
                            nm.g = g;
                            nm.h = h;
                            nm.f = h + g;
                            nm.flags = (nm.flags & 0x70) | REV[d] as u8;
                            self.push(j);
                        }
                    }
                    None => {
                        let (ds, dg) = ((c2.0 - start.0, c2.1 - start.1), (c2.0 - self.goal.0, c2.1 - self.goal.1));
                        if bound <= length(ds.0, ds.1) + length(dg.0, dg.1) {
                            continue;
                        }
                        let free = terrain_ok(self, c2);
                        let entity = free && self.entity_blocked(c2, ring_level);
                        if !free || (entity && !inside) {
                            self.node_mut(j).flags |= CLOSED;
                            continue;
                        }
                        let g = if entity { ASTAR_ENTITY_STEP } else { base } + n.g;
                        let h = self.hcell(c2);
                        let nm = self.node_mut(j);
                        nm.g = g;
                        nm.h = h;
                        nm.f = g + h;
                        nm.flags = (nm.flags & 0x70) | REV[d] as u8 | if entity { BLOCKED } else { 0 };
                        self.push(j);
                    }
                }
            }
        }
        if best == start {
            return Plan { path: Vec::new(), reached };
        }
        // Rebuild from the end, keeping a cell when the line from its parent to the last kept
        // cell is blocked by terrain (`0x141d1d070`) or crosses a ring.
        let parent = |s: &Self, c: Cell| -> Cell {
            let d = s.index(c).and_then(|i| s.node(i)).map_or(8, |n| (n.flags & 0xf) as usize).min(7);
            (c.0 + DXY[d].0, c.1 + DXY[d].1)
        };
        let mut path = vec![best];
        let mut kept = best;
        let mut c = parent(self, best);
        while c != start {
            let next = parent(self, c);
            let blocked = self.walk_blocked(next, kept) || Self::line_any(next, kept, |p| self.entity_blocked(p, ring_level));
            if blocked {
                if path.len() == 0x88 {
                    return Plan::default();
                }
                path.push(c);
                kept = c;
            }
            c = next;
        }
        if path.len() == 0x88 {
            return Plan::default();
        }
        path.push(start);
        Plan { path, reached }
    }
}

/// Plans a path from `start` to `goal` the way the client does.
///
/// `level` is the entity's clearance: cells whose grid value is at most `level` (value 5 counts
/// as 255) are walls; -1 turns the check off. A start cell that is itself a wall at `level > 0`
/// is planned again at level 0, which is what the client's fallback search came to.
pub fn plan(terrain: &dyn Walkable, start: Cell, goal: Cell, level: i32, params: &Params) -> Plan {
    let bounds = terrain.bounds();
    if let Some((w, h)) = bounds
        && (goal.0 < 0 || goal.1 < 0 || goal.0 >= w || goal.1 >= h)
    {
        return Plan::default();
    }
    SCRATCH.with(|s| {
        let mut s = s.borrow_mut();
        let s = &mut *s;
        if s.nodes.len() != SIDE * SIDE {
            s.nodes = vec![Node::default(); SIDE * SIDE];
            s.stamp = 0;
        }
        s.stamp = s.stamp.wrapping_add(1);
        if s.stamp == 0 {
            s.nodes.iter_mut().for_each(|n| n.stamp = 0);
            s.stamp = 1;
        }
        s.heap.clear();
        let max_ext = if params.radius < 0 {
            params.max_dist
        } else {
            params.max_dist.wrapping_add(params.radius as u32)
        };
        let a = (goal.0 - start.0, goal.1 - start.1);
        let d0 = length(start.0 - goal.0, start.1 - goal.1);
        let ellipse = (d0 * 2.0f32).clamp(20.0f32, 136.0f32);
        let mid = (
            (goal.0.wrapping_add(start.0)) / 2,
            (goal.1.wrapping_add(start.1)) / 2,
        );
        let r_m = ellipse * 0.433f32;
        let mut search = Search {
            terrain,
            bounds,
            scratch: s,
            start,
            goal,
            origin: (start.0 - HALF, start.1 - HALF),
            level,
            params: *params,
            a,
            goal2: max_ext.wrapping_mul(max_ext),
            max_ext: max_ext as f32,
            h_start: 0.0,
            cap: 5,
            ellipse,
            mid,
            mid_r2: (r_m * r_m) as u32,
        };
        if search.start_in_entity() {
            return search.astar();
        }
        search.h_start = search.hcell(start);
        search.cap = ((search.h_start * 0.33f32) as u32).clamp(5, 22);
        // The rings use the entity's size level (`local_658`), also when the terrain test falls
        // back to level 0 below.
        search.wall_off_obstacles(terrain.obstacles(), params.size_level.unwrap_or(level));
        if level > 0 && search.blocked_at(start) {
            search.level = 0;
            if search.blocked_at(start) {
                return Plan::default();
            }
        } else if level != -1 && search.blocked_at(start) {
            return Plan::default();
        }
        search.run()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::VecGrid;

    fn open() -> VecGrid {
        VecGrid::open(400, 400)
    }

    #[test]
    fn an_open_field_gives_one_straight_leg() {
        let plan = plan(&open(), (100, 100), (111, 111), 0, &Params::default());
        assert!(plan.reached);
        assert_eq!(plan.path, vec![(111, 111), (100, 100)]);
    }

    #[test]
    fn a_goal_behind_a_wall_ends_at_the_closest_reachable_cell() {
        // A wall along x = 120 for y < 200 and the goal behind it.
        let mut g = open();
        for y in 0..200 {
            g.set(120, y, false);
        }
        let plan = plan(&g, (100, 100), (130, 100), 0, &Params::default());
        assert!(!plan.reached);
        let end = plan.path[0];
        assert!(end.0 <= 119, "{end:?}");
        assert!((end.1 - 100).abs() <= 3, "{end:?}");
    }

    #[test]
    fn the_path_goes_around_an_obstacle() {
        let mut g = open();
        for y in 95..=105 {
            g.set(105, y, false);
        }
        let plan = plan(&g, (100, 100), (110, 100), 0, &Params::default());
        assert!(plan.reached);
        assert_eq!(plan.path[0], (110, 100));
        assert!(plan.path.len() >= 3, "{:?}", plan.path);
        // Every leg keeps clear of the wall.
        let pts: Vec<_> = plan.path.iter().rev().copied().collect();
        for w in pts.windows(2) {
            let (a, b) = (w[0], w[1]);
            let steps = (b.0 - a.0).abs().max((b.1 - a.1).abs()) * 2;
            for i in 0..=steps {
                let t = i as f32 / steps.max(1) as f32;
                let x = a.0 as f32 + 0.5 + (b.0 - a.0) as f32 * t;
                let y = a.1 as f32 + 0.5 + (b.1 - a.1) as f32 * t;
                assert!(g.walkable(x.floor() as i32, y.floor() as i32));
            }
        }
    }

    fn checkpoint_ring(g: &VecGrid, obstacles: &[Obstacle], start: Cell, goal: Cell) -> Plan {
        let t = crate::movement::WithObstacles { terrain: g, obstacles };
        plan(&t, start, goal, 0, &Params { size_level: Some(4), ..Params::default() })
    }

    #[test]
    fn a_start_inside_a_ring_walks_out_cell_by_cell_then_straight() {
        // A checkpoint (size 2) at (100, 100): ring radius 4 + 2 = 6. The way out with the
        // fewest ring cells is diagonal (3 cells at cost 21, straight out would be 4), kept cell
        // by cell because every line from inside crosses the ring.
        let obstacles = [Obstacle { cell: (100, 100), size: 2 }];
        let plan = checkpoint_ring(&open(), &obstacles, (100, 101), (100, 120));
        assert!(plan.reached);
        assert_eq!(plan.path, [(100, 120), (96, 105), (97, 104), (98, 103), (99, 102), (100, 101)]);
    }

    #[test]
    fn a_path_out_of_a_ring_does_not_cross_it_again() {
        let obstacles = [Obstacle { cell: (100, 100), size: 2 }];
        let plan = checkpoint_ring(&open(), &obstacles, (100, 101), (100, 80));
        assert!(plan.reached);
        assert_eq!(plan.path[0], (100, 80));
        let inside = |c: Cell| (c.0 - 100).pow(2) + (c.1 - 100).pow(2) < 36;
        let pts: Vec<Cell> = plan.path.iter().rev().copied().collect();
        let out = pts.iter().position(|&c| !inside(c)).expect("leaves the ring");
        for w in pts[out..].windows(2) {
            assert!(!Search::line_any(w[0], w[1], inside), "{:?} crosses the ring", w);
        }
    }

    #[test]
    fn a_goal_at_the_start_has_no_path() {
        let p = plan(&open(), (100, 100), (100, 100), 0, &Params::default());
        assert!(p.path.is_empty());
    }

    #[test]
    fn a_goal_outside_the_terrain_has_no_path() {
        let p = plan(&open(), (100, 100), (500, 100), 0, &Params::default());
        assert_eq!(p, Plan::default());
    }

    #[test]
    fn the_heap_pops_in_cost_order_and_keeps_msvcs_tie_order() {
        let mut h = Vec::new();
        for (k, f) in [
            (1u32, 3.0f32),
            (2, 1.0),
            (3, 2.0),
            (4, 1.0),
            (5, 3.0),
            (6, 2.0),
        ] {
            heap_push(&mut h, k, f);
        }
        let order: Vec<u32> =
            std::iter::from_fn(|| (!h.is_empty()).then(|| heap_pop(&mut h).0)).collect();
        let costs: Vec<f32> = order
            .iter()
            .map(|k| [0.0, 3.0, 1.0, 2.0, 1.0, 3.0, 2.0][*k as usize])
            .collect();
        assert!(costs.windows(2).all(|w| w[0] <= w[1]), "{order:?}");
        // MSVC's sift order for this input.
        assert_eq!(order, vec![2, 4, 3, 6, 1, 5]);
    }
}
