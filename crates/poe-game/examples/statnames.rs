//! Prints stat ids by network key, and the ids that hold a word. `statnames 236 3129 -- movement`
use poe_data::GameData;
fn main() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../..");
    let game = std::env::var_os("POE_GAME_DIR").map_or_else(|| root.join("GameFiles"), std::path::PathBuf::from);
    let data = GameData::open(&game, &root.join("third_party/dat-schema/schema-poe2.min.json")).unwrap();
    let t = data.table("Stats").unwrap();
    if let Ok(cls) = std::env::var("CLASS") {
        let tpl = poe_data::template::ObjectTemplate::load(&data, &cls).unwrap();
        for (k, v) in tpl.component("Stats").iter().chain(tpl.component("Pathfinding").iter()).chain(tpl.component("Positioned").iter()) {
            if ["speed", "velocity", "sprint", "action", "dodge", "scale", "size"].iter().any(|w| k.contains(w)) {
                println!("{k} = {v}");
            }
        }
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let split = args.iter().position(|a| a == "--").unwrap_or(args.len());
    for k in &args[..split] {
        let k: usize = k.parse().unwrap();
        println!("{k} {}", t.row(k - 1).unwrap().str("Id").unwrap());
    }
    for i in 0..t.len() {
        let id = t.row(i).unwrap().str("Id").unwrap();
        if args[(split + 1).min(args.len())..].iter().all(|w| id.contains(w.as_str())) && split < args.len() {
            println!("{} {id}", i + 1);
        }
    }
}
