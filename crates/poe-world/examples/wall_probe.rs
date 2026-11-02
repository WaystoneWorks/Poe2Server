//! Finds test spots on an area's generated collision grid (client side, as the instance server
//! builds it with `[terrain] generated_grid`) for the in-game wall scenarios (hub PLAN R2,
//! `scripts/game-test/`):
//!
//! `cargo run --release -p poe-world --example wall_probe -- <area id> <seed> [--count N] [--runs] [--layout FILE]`
//! (`--layout` also writes the generated layout in the DLL's `terrain_layout_<hash>.tsv` format)
//! (`POE_FORCE=<topology>:<orientation>` as in `area_gen`; unset = the natural layout, which is
//! what `transfer <area> <seed>` gets with `[terrain] force_layout`)
//!
//! Prints the tile hash and grid size, then per kind up to N spots in grid cells (the units of
//! the `teleport` command and of the DLL state file's `player.grid`):
//! - `open x y`: walkable within 12 cells all round;
//! - `wall x y keys free solid`: from (x, y) the W/A/S/D combination `keys` walks `free` cells
//!   in a straight line, then `solid` unwalkable cells follow (a wall or cliff edge 5+ cells
//!   wide across the line);
//! - with `--runs`, after each `open` line: `runs x y K:n ...`, the free cells (up to 80) from
//!   the spot along each of the 8 key directions (room for a roll plus a sprint);
//! - `narrow x y keys width`: a passage `width` cells wide; walking `keys` from (x, y) goes
//!   through it after 15 cells and comes out in the open after 30.
//!
//! Walkable = layer 0 non-zero (`poe-game` `Walkable`). Directions: W = grid (+1, +1),
//! S = (-1, -1), D = (+1, -1), A = (-1, +1) (the `0x63` vectors, hub docs/protocol/movement.md).
use std::path::Path;

use poe_world::{area::Forced, collision::CellGrid, generate::CachedFiles, rng::TerrainRng};

const DIRS: [(&str, (f64, f64)); 8] = [
    ("W", (1.0, 1.0)),
    ("WD", (1.0, 0.0)),
    ("D", (1.0, -1.0)),
    ("SD", (0.0, -1.0)),
    ("S", (-1.0, -1.0)),
    ("AS", (-1.0, 0.0)),
    ("A", (-1.0, 1.0)),
    ("WA", (0.0, 1.0)),
];

struct G<'a>(&'a CellGrid);

impl G<'_> {
    fn walk(&self, x: f64, y: f64) -> bool {
        let (x, y) = (x.round() as i64, y.round() as i64);
        x >= 0 && y >= 0 && (x as usize) < self.0.cells_x && (y as usize) < self.0.cells_y && self.0.get(0, x as usize, y as usize) != 0
    }
    fn clear(&self, x: f64, y: f64, r: i32) -> bool {
        (-r..=r).all(|dy| (-r..=r).all(|dx| dx * dx + dy * dy > r * r || self.walk(x + dx as f64, y + dy as f64)))
    }
    /// Walkable cells in a row from (x, y) along `u` (unit), not counting the start.
    fn run(&self, (x, y): (f64, f64), (ux, uy): (f64, f64), max: i32) -> i32 {
        (1..=max).find(|&k| !self.walk(x + ux * k as f64, y + uy * k as f64)).map_or(max, |k| k - 1)
    }
}

fn unit((x, y): (f64, f64)) -> (f64, f64) {
    let n = (x * x + y * y).sqrt();
    (x / n, y / n)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let area = args.next().ok_or("usage: wall_probe <area> <seed> [--count N]")?;
    let seed: u32 = args.next().ok_or("seed")?.parse()?;
    let mut count = 5;
    let mut layout = None;
    let mut runs = false;
    while let Some(a) = args.next() {
        if a == "--count" {
            count = args.next().ok_or("--count N")?.parse()?;
        } else if a == "--runs" {
            runs = true;
        } else if a == "--layout" {
            layout = Some(args.next().ok_or("--layout FILE")?);
        }
    }
    let forced = std::env::var("POE_FORCE").ok().map(|v| {
        let (t, o) = v.split_once(':').unwrap_or((&v, ""));
        Forced { topology: t.parse().ok(), orientation: o.parse().ok() }
    });
    let gd = poe_data::GameData::open(Path::new("GameFiles"), Path::new("third_party/dat-schema/schema-poe2.min.json"))?;
    let files = CachedFiles::new(gd.files());
    let g = poe_world::area::generate_with(&gd, &area, &mut TerrainRng::new(seed), forced)?;
    let grid = poe_world::collision::build(&g.map, &files, g.doodads.iter().chain(&g.tile_objects), poe_world::doodads::Side::Client)?;
    println!("{area} seed {seed} tile_hash {} grid {}x{}", g.map.tile_hash(), grid.cells_x, grid.cells_y);
    if let Some(path) = layout {
        std::fs::write(&path, g.map.layout_tsv())?;
        println!("layout written to {path}");
    }
    let gr = G(&grid);
    let (w, h) = (grid.cells_x as i32, grid.cells_y as i32);
    // Spots from the middle out, so the first ones are not all in one corner.
    let mut cells: Vec<(f64, f64)> = (0..h).step_by(3).flat_map(|y| (0..w).step_by(3).map(move |x| (x as f64, y as f64))).collect();
    let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);
    cells.sort_by(|a, b| ((a.0 - cx).hypot(a.1 - cy)).total_cmp(&(b.0 - cx).hypot(b.1 - cy)));
    let far = |picked: &[(f64, f64)], p: (f64, f64)| picked.iter().all(|q| (q.0 - p.0).hypot(q.1 - p.1) > 60.0);

    let mut open = Vec::new();
    for &p in &cells {
        if open.len() < count && far(&open, p) && gr.clear(p.0, p.1, 12) {
            open.push(p);
            println!("open {} {}", p.0, p.1);
            if runs {
                let r: Vec<String> = DIRS.iter().map(|(k, d)| format!("{k}:{}", gr.run(p, unit(*d), 80))).collect();
                println!("runs {} {} {}", p.0, p.1, r.join(" "));
            }
        }
    }
    let mut walls = Vec::new();
    'wall: for &p in &cells {
        if walls.len() >= count || !far(&walls, p) || !gr.clear(p.0, p.1, 3) {
            continue;
        }
        for (keys, d) in DIRS {
            let u = unit(d);
            let free = gr.run(p, u, 40);
            if !(12..=30).contains(&free) {
                continue;
            }
            // Solid for 6 cells along the line and 5 cells to either side of it.
            let n = (-u.1, u.0);
            let solid = (1..=6).all(|j| {
                let k = (free + j) as f64;
                (-5..=5).all(|s| !gr.walk(p.0 + u.0 * k + n.0 * s as f64, p.1 + u.1 * k + n.1 * s as f64))
            });
            // The line itself has room on both sides (not a corridor along the wall).
            let roomy = (2..free - 2).all(|k| gr.clear(p.0 + u.0 * k as f64, p.1 + u.1 * k as f64, 2));
            if solid && roomy {
                walls.push(p);
                println!("wall {} {} {keys} {free} 6", p.0, p.1);
                continue 'wall;
            }
        }
    }
    let mut narrow = Vec::new();
    'narrow: for &c in &cells {
        if narrow.len() >= count || !far(&narrow, c) || !gr.walk(c.0, c.1) {
            continue;
        }
        for (keys, d) in DIRS {
            let u = unit(d);
            let n = (-u.1, u.0);
            let width = gr.run(c, n, 12) + gr.run(c, (-n.0, -n.1), 12) + 1;
            if !(2..=6).contains(&width) {
                continue;
            }
            // A clear line through it, open ground 15 cells before and after.
            let line = (-15..=15).all(|k| gr.walk(c.0 + u.0 * k as f64, c.1 + u.1 * k as f64));
            let ends = gr.clear(c.0 - u.0 * 15.0, c.1 - u.1 * 15.0, 4) && gr.clear(c.0 + u.0 * 15.0, c.1 + u.1 * 15.0, 4);
            if line && ends {
                let s = ((c.0 - u.0 * 15.0).round(), (c.1 - u.1 * 15.0).round());
                narrow.push(c);
                println!("narrow {} {} {keys} {width}", s.0, s.1);
                continue 'narrow;
            }
        }
    }
    Ok(())
}
