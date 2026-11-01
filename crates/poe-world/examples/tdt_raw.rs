//! Prints a `.tdt`'s inheritance flags and raw body bytes, and one sub-tile's decoded planes
//! (layer 0 nibbles): `tdt_raw <game path> [sub-tile] [layer]`.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let files = poe_data::GameFiles::open(std::path::Path::new("GameFiles"))?;
    let a: Vec<String> = std::env::args().skip(1).collect();
    let t = poe_world::tdt::Tdt::parse(&files.read(&a[0].to_lowercase())?)?;
    println!("{}: inherits {:?} inherit_flags {:#x} body_flags {:#x} hull_masks {:?} rest {:02x?}", a[0], t.inherits, t.inherit_flags, t.body_flags, t.hull_masks, &t.rest[..t.rest.len().min(64)]);
    let Some(i) = a.get(1).and_then(|s| s.parse::<usize>().ok()) else { return Ok(()) };
    let layer: u32 = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
    let s = &t.subtiles[i];
    let (f, b1) = (s.flags, s.b1);
    let w = (if f & 0x40 != 0 { 27 } else { 23 }) + if b1 & 1 != 0 { 4 } else { 0 };
    let h = (if f & 0x80 != 0 { 27 } else { 23 }) + if f & 0x20 != 0 { 4 } else { 0 };
    let raw = poe_world::tdt::lz4_decode(&s.list).unwrap_or_default();
    println!("sub {i} flags {f:#x} b1 {b1:#x} plane {w}x{h} decoded {} ({} planes)", raw.len(), raw.len() as f32 / (w * h) as f32);
    for (k, p) in raw.chunks(w * h).enumerate() {
        println!(" plane {k}");
        for y in 0..h {
            println!("  {:3} {}", y, (0..w).map(|x| format!("{:x}", (p[y * w + x] >> (4 * layer)) & 15)).collect::<String>());
        }
    }
    Ok(())
}
