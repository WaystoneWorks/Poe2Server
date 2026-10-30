//! Prints `.tdt` headers: `cargo run -p poe-world --example tdt_info -- <path.tdt>...`
//! (game paths, read from GameFiles).

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let files = poe_data::GameFiles::open(std::path::Path::new("GameFiles"))?;
    for path in std::env::args().skip(1) {
        let bytes = files.read(&path.to_lowercase())?;
        match poe_world::tdt::Tdt::parse(&bytes) {
            Ok(t) => println!(
                "{path}: v{} feature {:?} size {:?} inherits {:?} edges {:?} grounds {:?} unk {:?} dist {:?} rest {} bytes orientation_mask {:#x} hull_masks {:?}",
                t.version,
                t.feature,
                t.size,
                t.inherits,
                t.edges
                    .iter()
                    .map(|e| e.as_deref().unwrap_or("-").rsplit('/').next().unwrap_or(""))
                    .collect::<Vec<_>>(),
                t.grounds
                    .iter()
                    .map(|e| e.as_deref().unwrap_or("-").rsplit('/').next().unwrap_or(""))
                    .collect::<Vec<_>>(),
                t.unk,
                t.edge_distances,
                t.rest.len(), t.orientation_mask, t.hull_masks
            ),
            Err(e) => println!("{path}: {e}"),
        }
        if let Ok(t) = poe_world::tdt::Tdt::parse(&bytes) {
            println!("  tgt {:?} tmd {:?} body_flags {:#06x} object_group {}", t.tgt, t.tmd, t.body_flags, t.object_group);
            for (i, st) in t.subtiles.iter().enumerate() {
                let map = match &st.map {
                    None => "none".to_string(),
                    Some(m) if m.len() <= 1 => format!("uniform {:?}", m.first()),
                    Some(m) => format!("{} codes", m.len()),
                };
                println!("  sub {i}: flags {:#04x} b1 {} map {map} list {} bytes", st.flags, st.b1, st.list.len());
                if std::env::var("TDT_MAP").ok().and_then(|v| v.parse::<usize>().ok()) == Some(i) {
                    if let Some(m) = st.map.as_ref().filter(|m| m.len() == 529) {
                        for row in m.chunks(23) {
                            println!("    {}", row.iter().map(|c| format!("{c:02x}")).collect::<Vec<_>>().join(" "));
                        }
                    }
                }
            }
        }
    }
    Ok(())
}
