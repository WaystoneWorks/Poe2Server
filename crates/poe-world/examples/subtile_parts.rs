//! Prints the per-sub-tile bytes of `.tdt` files with body flag 0x200 (one `.tgt` per sub-tile):
//! `subtile_parts <game path>...`. Per sub-tile: index, (x, y), `b1`, `parts` (hex).
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let files = poe_data::GameFiles::open(std::path::Path::new("GameFiles"))?;
    for p in std::env::args().skip(1) {
        let t = poe_world::tdt::Tdt::parse(&files.read(&p.to_lowercase())?)?;
        println!("{p}: v{} size {:?} body {:#x} tgt {:?} tmd {:?}", t.version, t.size, t.body_flags, t.tgt, t.tmd);
        let w = t.size.0.max(1) as usize;
        for (i, s) in t.subtiles.iter().enumerate() {
            let parts = s.parts.map(|b| format!("{:02x} {:02x} {:02x}", b[0], b[1], b[2])).unwrap_or_default();
            println!("  {i} ({},{}) b1 {:#04x} parts {parts}", i % w, i / w, s.b1);
        }
    }
    Ok(())
}
