//! Counts `.tmd` files by format (text `version`, or the first byte of a binary file), and
//! writes the binary ones' paths: `tmd_scan <out list>`.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let files = poe_data::GameFiles::open(std::path::Path::new("GameFiles"))?;
    let out = std::env::args().nth(1).unwrap_or_else(|| "tmd-binary.txt".into());
    let mut kinds = std::collections::BTreeMap::<String, usize>::new();
    let mut bin = String::new();
    for p in files.list() {
        if !p.to_lowercase().ends_with(".tmd") {
            continue;
        }
        let Ok(b) = files.read(&p.to_lowercase()) else { continue };
        let text = String::from_utf16_lossy(&b.chunks_exact(2).take(16).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>());
        let k = if text.trim_start_matches(|c: char| c == '\u{feff}' || c.is_whitespace()).starts_with("version") { "text".to_string() } else {
            bin.push_str(&format!("{p}\t{}\n", b.len()));
            format!("binary v{}", b.first().copied().unwrap_or(0))
        };
        *kinds.entry(k).or_default() += 1;
    }
    std::fs::write(&out, bin)?;
    println!("{kinds:?}");
    Ok(())
}
