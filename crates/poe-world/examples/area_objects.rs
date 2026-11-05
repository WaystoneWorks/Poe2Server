//! Lists the objects the generator places on the server side for one area: every
//! `Side::Server` object of a room or tile that is not a plain doodad.
//!
//! `cargo run --release -p poe-world --example area_objects -- <area> <seed> [regex-free filter]`

use std::path::Path;

use poe_data::GameData;
use poe_world::{area, doodads::Side};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let area_id = args.next().ok_or("usage: area_objects <area> <seed> [filter]")?;
    let seed: u32 = args.next().ok_or("seed")?.parse()?;
    let filter = args.next().map(|f| f.to_ascii_lowercase());
    let gd = GameData::open(Path::new("GameFiles"), Path::new("third_party/dat-schema/schema-poe2.min.json"))?;
    let g = area::generate(&gd, &area_id, seed)?;
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
