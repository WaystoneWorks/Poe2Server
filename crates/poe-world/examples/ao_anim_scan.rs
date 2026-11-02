//! Lists the room doodads of client exports whose `.ao` declares an `AnimationController` and a
//! `Hull` with triangles, with their angle: `ao_anim_scan <export dir>...`. One line per doodad
//! (`export x y angle path`) with the angle away from 0 (mod 2pi); a hull stamped unrotated
//! differs from a rotated one only then.
use std::collections::HashMap;
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let files = poe_data::GameFiles::open(Path::new("GameFiles"))?;
    let mut kind: HashMap<String, bool> = HashMap::new();
    for dir in std::env::args().skip(1) {
        let name = Path::new(&dir)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let Ok(tsv) = std::fs::read_to_string(Path::new(&dir).join("doodads.tsv")) else {
            continue;
        };
        for line in tsv.lines().filter(|l| !l.starts_with('#')) {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() < 5 {
                continue;
            }
            let path = f[4].to_ascii_lowercase();
            let anim = *kind.entry(path.clone()).or_insert_with(|| {
                files.read(&path).map_or(false, |b| {
                    let u: Vec<u16> = b
                        .chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]]))
                        .collect();
                    let t = String::from_utf16_lossy(&u);
                    t.contains("AnimationController")
                        && t.contains("Hull")
                        && (t.contains("walk_tri") || t.contains("proj_tri"))
                })
            });
            let angle: f32 = f[2].parse().unwrap_or(0.0);
            let a = angle.rem_euclid(std::f32::consts::TAU);
            if anim && a.min(std::f32::consts::TAU - a) > 0.01 {
                println!("{name}\t{}\t{}\t{angle}\t{}", f[0], f[1], f[4]);
            }
        }
    }
    Ok(())
}
