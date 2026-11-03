//! Game file lookup for RE: `gamefile find WORD..` lists the paths containing every word;
//! `gamefile cat PATH [GREP]` prints a UTF-16 text file (the lines containing GREP).
use poe_data::GameData;
fn main() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../..");
    let game = std::env::var_os("POE_GAME_DIR").map_or_else(|| root.join("GameFiles"), std::path::PathBuf::from);
    let data = GameData::open(&game, &root.join("third_party/dat-schema/schema-poe2.min.json")).unwrap();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("find") => {
            for p in data.files().list() {
                if args[1..].iter().all(|w| p.contains(w.as_str())) {
                    println!("{p}");
                }
            }
        }
        Some("cat") => {
            let bytes = data.files().read(&args[1]).unwrap();
            let words: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            let text = String::from_utf16_lossy(&words);
            for l in text.lines() {
                if args.get(2).is_none_or(|g| l.contains(g.as_str())) {
                    println!("{l}");
                }
            }
        }
        _ => eprintln!("gamefile find WORD.. | cat PATH [GREP]"),
    }
}
