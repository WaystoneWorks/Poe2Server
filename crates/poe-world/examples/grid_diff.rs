//! The server's generated collision grid against the client's own (`terrain_grid_<tile>.bin`,
//! exported by the client DLL): cells whose layer differs, and both grids around a cell.
//!
//! `grid_diff <area> <seed> <client grid .bin> [--layer L] [--at X Y [R]]` (run from the hub root;
//! `POE_FORCE=topology:orientation` as in `wall_probe`). Prints the tile hash, the count of
//! differing cells per layer, the first differing cells, and with `--at` the window of layer L
//! (default 0) values, server row above client row.

use std::path::Path;

use poe_world::{area::Forced, collision::CellGrid, generate::CachedFiles, rng::TerrainRng};

struct Dump {
    w: usize,
    h: usize,
    row: usize,
    layers: Vec<Vec<u8>>,
}

impl Dump {
    fn read(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let b = std::fs::read(path)?;
        if &b[..4] != b"PWG1" {
            return Err("not a PWG1 grid".into());
        }
        let u = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as usize;
        let (w, h, row, n) = (u(4), u(8), u(12), u(16));
        let layers = (0..n).map(|i| b[20 + i * row * h..20 + (i + 1) * row * h].to_vec()).collect();
        Ok(Self { w, h, row, layers })
    }
    fn get(&self, l: usize, x: usize, y: usize) -> u8 {
        let v = self.layers[l][y * self.row + x / 2];
        if x % 2 == 0 { v & 15 } else { v >> 4 }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() < 3 {
        return Err("usage: grid_diff <area> <seed> <client grid .bin> [--layer L] [--at X Y [R]]".into());
    }
    let (area, seed, dump) = (&a[0], a[1].parse::<u32>()?, Dump::read(&a[2])?);
    let pos = |k: &str| a.iter().position(|x| x == k);
    let layer = pos("--layer").map_or(Ok(0), |i| a[i + 1].parse())?;
    let at = pos("--at").map(|i| {
        let n = |j: usize| a.get(i + j).and_then(|v| v.parse::<i64>().ok());
        (n(1).unwrap_or(0), n(2).unwrap_or(0), n(3).unwrap_or(6))
    });
    let forced = std::env::var("POE_FORCE").ok().map(|v| {
        let (t, o) = v.split_once(':').unwrap_or((&v, ""));
        Forced { topology: t.parse().ok(), orientation: o.parse().ok() }
    });
    let gd = poe_data::GameData::open(Path::new("GameFiles"), Path::new("third_party/dat-schema/schema-poe2.min.json"))?;
    let files = CachedFiles::new(gd.files());
    let g = poe_world::area::generate_with(&gd, area, &mut TerrainRng::new(seed), forced)?;
    let grid: CellGrid = poe_world::collision::build(&g.map, &files, g.doodads.iter().chain(&g.tile_objects), poe_world::doodads::Side::Client)?;
    println!("{area} seed {seed} tile_hash {} server {}x{} client {}x{}", g.map.tile_hash(), grid.cells_x, grid.cells_y, dump.w, dump.h);
    let (w, h) = (grid.cells_x.min(dump.w), grid.cells_y.min(dump.h));
    for l in 0..dump.layers.len() {
        let mut first = Vec::new();
        let mut n = 0usize;
        for y in 0..h {
            for x in 0..w {
                if grid.get(l, x, y) != dump.get(l, x, y) {
                    n += 1;
                    if first.len() < 8 {
                        first.push(format!("({x},{y}) s{} c{}", grid.get(l, x, y), dump.get(l, x, y)));
                    }
                }
            }
        }
        println!("layer {l}: {n} cells differ; first {}", first.join(" "));
    }
    if let Some((cx, cy, r)) = at {
        println!("layer {layer} around ({cx},{cy}); per row: y, server values, client values (x from {})", cx - r);
        for y in cy - r..=cy + r {
            let val = |f: &dyn Fn(usize, usize) -> u8| -> String {
                (cx - r..=cx + r)
                    .map(|x| if x < 0 || y < 0 || x as usize >= w || y as usize >= h { '#' } else { char::from_digit(f(x as usize, y as usize) as u32, 16).unwrap() })
                    .collect()
            };
            println!("{y:5} s {}  c {}", val(&|x, y| grid.get(layer, x, y)), val(&|x, y| dump.get(layer, x, y)));
        }
    }
    Ok(())
}
