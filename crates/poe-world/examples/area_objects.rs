//! Lists the objects the generator places on the server side.
//!
//! `area_objects <area> <seed> [filter]`: one area, every `Side::Server` object of a room or tile
//! that is not a plain doodad.
//! `area_objects --survey <seed> <out.tsv>`: every area reachable by `WorldAreas.Connections`
//! from the towns of acts 1-4 and the interludes; one line per travel object, with the room's
//! node tags and the area's connections, then one `GATE` line per gate with its target and rule.
//!
//! Run from the hub root (it opens `GameFiles`).

use std::{collections::BTreeSet, fmt::Write as _, io::Write as _, path::Path, sync::Mutex};

use poe_data::GameData;
use poe_world::{area, doodads::Side, generate::CachedFiles, objects, rng::TerrainRng};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let first = args.next().ok_or("usage: area_objects <area> <seed> [filter] | --survey <seed> <out.tsv>")?;
    let gd = GameData::open(Path::new("GameFiles"), Path::new("third_party/dat-schema/schema-poe2.min.json"))?;
    if first == "--survey" {
        let seed: u32 = args.next().ok_or("seed")?.parse()?;
        let out = args.next().ok_or("out")?;
        return survey(&gd, seed, &out);
    }
    let seed: u32 = args.next().ok_or("seed")?.parse()?;
    let filter = args.next().map(|f| f.to_ascii_lowercase());
    let g = area::generate(&gd, &first, seed)?;
    println!("graph {} doodads {} tile_objects {}", g.graph, g.doodads.len(), g.tile_objects.len());
    for (src, list) in [("room", &g.doodads), ("tile", &g.tile_objects)] {
        for d in list {
            if !d.exists(Side::Server) {
                continue;
            }
            let k = d.kind.to_ascii_lowercase();
            if let Some(f) = &filter {
                if !k.contains(f.as_str()) {
                    continue;
                }
            } else if k.ends_with("/doodad") || k.contains("doodadnoverlap") || k.contains("/environment/") {
                continue;
            }
            println!("{src}\t{}\t{}\t{:.3}\t{}\t{}", d.x, d.y, d.angle, d.kind, d.path);
        }
    }
    Ok(())
}

/// A travel object's type, by words in its metadata path.
fn is_travel(kind: &str) -> bool {
    let k = kind.to_ascii_lowercase();
    ["areatransition", "waypoint", "checkpoint", "healingwell", "portal", "teleport"].iter().any(|w| k.contains(w))
}

fn survey(gd: &GameData, seed: u32, out: &str) -> Result<(), Box<dyn std::error::Error>> {
    let table = gd.table("WorldAreas")?;
    let rows: Vec<_> = table.rows().collect();
    let conns = |i: usize| -> Vec<usize> { rows[i].get("Connections").ok().and_then(|v| v.as_list().map(|l| l.iter().filter_map(|x| x.as_row()).collect())).unwrap_or_default() };
    // Start from the towns of the campaign acts and the interludes, walk the connections.
    let mut seen = BTreeSet::new();
    let mut todo: Vec<usize> = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        let id = r.str("Id")?;
        if r.bool("IsTown")? && ["G1_", "G2_", "G3_", "G4_", "P1_", "P2_", "P3_"].iter().any(|p| id.starts_with(p)) {
            todo.push(i);
        }
    }
    while let Some(i) = todo.pop() {
        if seen.insert(i) {
            todo.extend(conns(i));
        }
    }
    let list: Vec<usize> = seen.into_iter().collect();
    let next = Mutex::new(0usize);
    let files = CachedFiles::new(gd.files());
    let lines = Mutex::new(std::fs::File::create(out)?);
    let ids: Vec<(String, String, i64, bool, bool)> = rows.iter().map(|r| (r.str("Id").unwrap_or_default(), r.str("Name").unwrap_or_default(), r.int("Act").unwrap_or(0), r.bool("IsTown").unwrap_or(false), r.bool("HasWaypoint").unwrap_or(false))).collect();
    let conn_ids: Vec<Vec<String>> = (0..rows.len()).map(|i| conns(i).into_iter().map(|c| ids[c].0.clone()).collect()).collect();
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| {
                loop {
                    let k = {
                        let mut n = next.lock().unwrap();
                        let k = *n;
                        *n += 1;
                        k
                    };
                    let Some(&i) = list.get(k) else { return };
                    let (id, _name, act, town, wp) = &ids[i];
                    let mut text = String::new();
                    let head = format!("{id}\t{act}\t{town}\t{wp}\t{}", conn_ids[i].join(","));
                    let start = std::time::Instant::now();
                    match area::generate_with_files(gd, &files, id, &mut TerrainRng::new(seed), None) {
                        Err(e) => writeln!(text, "{head}\tERROR\t{e}").unwrap(),
                        Ok(g) => {
                            let mut n = 0;
                            for (src, l) in [("room", &g.doodads), ("tile", &g.tile_objects)] {
                                for d in l {
                                    if !d.exists(Side::Server) || !is_travel(&d.kind) {
                                        continue;
                                    }
                                    n += 1;
                                    let tags: Vec<String> = g
                                        .map
                                        .tagged_rooms
                                        .iter()
                                        .filter(|r| {
                                            // Tiles are 23 cells.
                                            let (x, y) = (d.x.div_euclid(23), d.y.div_euclid(23));
                                            x >= r.origin.0 && y >= r.origin.1 && x < r.origin.0 + r.size.0 && y < r.origin.1 + r.size.1
                                        })
                                        .map(|r| format!("{}[{}]", r.room_type, r.tags.join("+")))
                                        .collect();
                                    writeln!(text, "{head}\tOBJ\t{src}\t{}\t{}\t{:.3}\t{}\t{}\t{}", d.x, d.y, d.angle, d.kind, d.path, tags.join(";")).unwrap();
                                }
                            }
                            match objects::travel_objects(gd, id, &g) {
                                Err(e) => writeln!(text, "{head}\tOBJERROR\t{e}").unwrap(),
                                Ok(list) => {
                                    for o in list {
                                        if o.class == objects::Class::Gate {
                                            writeln!(text, "{head}\tGATE\t{}\t{}\t{:?}\t{:?}\t{}", o.x, o.y, o.rule, o.target, o.kind).unwrap();
                                        } else {
                                            writeln!(text, "{head}\tOTHER\t{}\t{}\t{:?}\t{}\t{}", o.x, o.y, o.class, o.kind, o.ao).unwrap();
                                        }
                                    }
                                }
                            }
                            if n == 0 {
                                writeln!(text, "{head}\tNONE\t{}", g.graph).unwrap();
                            }
                        }
                    }
                    eprintln!("{id} {:.1}s", start.elapsed().as_secs_f32());
                    let mut f = lines.lock().unwrap();
                    f.write_all(text.as_bytes()).unwrap();
                    f.flush().unwrap();
                }
            });
        }
    });
    Ok(())
}
