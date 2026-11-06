//! Compares the port's corner lattice with the client's final lattice of batch exports
//! (`lattice.bin`: u32 points per row, u32 rows, then 3 bytes per point: ground index, height,
//! flags; flag 8 marks an unwalkable corner, which the collision grid reads, `0x141d1e2d0`):
//!
//! `cargo run --release -p poe-world --example check_lattice -- <export or run dir>... [--list N]`
//!
//! Per export: corners whose flag 8 differs (port set / client set), the first `N` of them with
//! both flag bytes and the port's ground, and the flag bits that differ anywhere.
use std::path::{Path, PathBuf};

use poe_data::GameData;
use poe_world::{area::Forced, generate::CachedFiles, rng::TerrainRng};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut dirs = Vec::new();
    let mut list = 10usize;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--list" => list = args.next().ok_or("--list needs a number")?.parse()?,
            _ => dirs.push(PathBuf::from(a)),
        }
    }
    let mut exports = Vec::new();
    for d in dirs {
        if d.join("lattice.bin").exists() {
            exports.push(d);
        } else {
            let e = if d.join("exports").is_dir() { d.join("exports") } else { d };
            let mut v: Vec<PathBuf> = std::fs::read_dir(e)?.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.join("lattice.bin").exists()).collect();
            v.sort();
            exports.extend(v);
        }
    }
    let gd = GameData::open(Path::new("GameFiles"), Path::new("third_party/dat-schema/schema-poe2.min.json"))?;
    let files = CachedFiles::new(gd.files());
    for dir in exports {
        let meta = std::fs::read_to_string(dir.join("meta.txt"))?;
        let get = |k: &str| meta.lines().find_map(|l| l.strip_prefix(k).and_then(|x| x.strip_prefix('\t'))).map(str::to_string);
        let (Some(area), Some(seed), Some(t), Some(o)) = (get("area"), get("seed"), get("topology"), get("orientation")) else { continue };
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        let mut rng = TerrainRng::new(seed.parse()?);
        let forced = Forced { topology: Some(t.parse()?), orientation: Some(o.parse()?) };
        let g = match poe_world::area::generate_with_files(&gd, &files, &area, &mut rng, Some(forced)) {
            Ok(g) => g,
            Err(e) => {
                println!("ERR  {name} {e}");
                continue;
            }
        };
        let raw = std::fs::read(dir.join("lattice.bin"))?;
        let (pw, ph) = (u32::from_le_bytes(raw[0..4].try_into()?) as usize, u32::from_le_bytes(raw[4..8].try_into()?) as usize);
        let (mw, mh) = (g.map.width as usize + 1, g.map.height as usize + 1);
        if (pw, ph) != (mw, mh) {
            println!("SIZE {name} client {pw}x{ph} port {mw}x{mh}");
            continue;
        }
        // `LATTICE_GROUNDS=1`: client ground index against the port's ground, with flag 8 on each side.
        if std::env::var_os("LATTICE_GROUNDS").is_some() {
            let mut m: std::collections::BTreeMap<(u8, String, bool, bool), usize> = Default::default();
            for y in 0..ph {
                for x in 0..pw {
                    let r = &raw[8 + (y * pw + x) * 3..];
                    let p = g.map.point_index(x as i32, y as i32).map(|i| &g.map.lattice[i]);
                    let gname = p.and_then(|p| p.ground.as_deref()).map_or("-", |g| g.rsplit('/').next().unwrap_or("")).to_string();
                    *m.entry((r[0], gname, p.is_some_and(|p| p.flags & 8 != 0), r[2] & 8 != 0)).or_default() += 1;
                }
            }
            for ((ci, pg, pf, cf), n) in m {
                println!("  GROUND client#{ci} port {pg} port8 {pf} client8 {cf}: {n}");
            }
        }
        let (mut port_only, mut client_only, mut bits) = (0, 0, 0u8);
        let mut shown = 0;
        for y in 0..ph {
            for x in 0..pw {
                let theirs = raw[8 + (y * pw + x) * 3 + 2];
                let ours = g.map.point_index(x as i32, y as i32).map_or(0, |i| g.map.lattice[i].flags);
                bits |= theirs ^ ours;
                let (a, b) = (ours & 8 != 0, theirs & 8 != 0);
                if a != b {
                    if a { port_only += 1 } else { client_only += 1 }
                    if shown < list {
                        shown += 1;
                        let ground = g.map.point_index(x as i32, y as i32).and_then(|i| g.map.lattice[i].ground.clone()).unwrap_or_default();
                        println!("  {name} corner ({x},{y}) port {ours:#04x} client {theirs:#04x} ground {}", ground.rsplit('/').next().unwrap_or(""));
                    }
                }
            }
        }
        // `LATTICE_COL=x`: one column, client ground index and flags, port ground and flags.
        if let Some(cx) = std::env::var("LATTICE_COL").ok().and_then(|v| v.parse::<usize>().ok()) {
            for y in 0..ph.min(30) {
                let i = 8 + (y * pw + cx) * 3;
                let p = g.map.point_index(cx as i32, y as i32).map(|k| &g.map.lattice[k]);
                println!("  ({cx},{y}) client ground#{} h{} {:#04x} | port {} h{} {:#04x}", raw[i], raw[i + 1] as i8, raw[i + 2], p.and_then(|p| p.ground.as_deref()).map_or("-", |g| g.rsplit('/').next().unwrap_or("")), p.map_or(0, |p| p.height), p.map_or(0, |p| p.flags));
            }
        }
        // `LATTICE_ROW=y`: one row, as `LATTICE_COL`.
        if let Some(cy) = std::env::var("LATTICE_ROW").ok().and_then(|v| v.parse::<usize>().ok()) {
            for x in 0..pw.min(30) {
                let i = 8 + (cy * pw + x) * 3;
                let p = g.map.point_index(x as i32, cy as i32).map(|k| &g.map.lattice[k]);
                println!("  ({x},{cy}) client ground#{} {:#04x} | port {} {:#04x} tile {:?}", raw[i], raw[i + 2], p.and_then(|p| p.ground.as_deref()).map_or("-", |g| g.rsplit('/').next().unwrap_or("")), p.map_or(0, |p| p.flags), g.map.get(x as u32, cy as u32).map(|c| c.path.rsplit('/').next().unwrap_or("").to_string()));
            }
        }
        if std::env::var_os("LATTICE_MAP").is_some() {
            // `.` both walkable, `#` both unwalkable, `c` client only, `p` port only.
            for y in 0..ph {
                let row: String = (0..pw).map(|x| {
                    let theirs = raw[8 + (y * pw + x) * 3 + 2] & 8 != 0;
                    let ours = g.map.point_index(x as i32, y as i32).is_some_and(|i| g.map.lattice[i].flags & 8 != 0);
                    match (ours, theirs) { (false, false) => '.', (true, true) => '#', (false, true) => 'c', (true, false) => 'p' }
                }).collect();
                println!("{y:4} {row}");
            }
        }
        println!("{} {name} unwalkable: port only {port_only}, client only {client_only}; differing flag bits {bits:#04x}", if port_only + client_only == 0 { "ok  " } else { "DIFF" });
    }
    Ok(())
}
