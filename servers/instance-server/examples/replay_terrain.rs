//! The terrain the server's movement sees in an area, for the offline replay
//! (`scripts/re/first_divergence.py` in the hub, `poe-game/examples/sim_replay.rs`):
//!
//! `cargo run --release -p instance-server --example replay_terrain -- <area id | 0xHASH16> <seed> <out dir> [--config server.toml]`
//!
//! Writes `<out dir>/<area>_<seed>.pwg` (the collision grid, `PWG1`) and
//! `<out dir>/<area>_<seed>.objects` (one spawned object per line: `x y reach targeting
//! obstacle-size-or--1 type`), and prints the
//! area id with the skill ids the server gives the player (`<area> move=0x.. interact=0x.. roll=1,..`). The config (default: `POE_CONFIG` or `config/server.toml`) gives the game files and
//! the travel objects the server skips.

use poe_data::GameData;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let pos: Vec<&String> = a.iter().enumerate().filter(|(i, s)| !s.starts_with("--") && (*i == 0 || a[i - 1] != "--config")).map(|(_, s)| s).collect();
    let [area, seed, out] = pos[..] else {
        return Err("usage: replay_terrain <area id | 0xHASH16> <seed> <out dir> [--config server.toml]".into());
    };
    let config = match a.iter().position(|s| s == "--config") {
        Some(i) => poe_config::Config::load(&PathBuf::from(&a[i + 1]))?,
        None => poe_config::Config::load_default()?,
    };
    let data = GameData::open(&config.game.game_dir, &config.game.schema)?;
    let area = match area.strip_prefix("0x") {
        Some(h) => {
            let h = u16::from_str_radix(h, 16)?;
            poe_data::views::WorldArea::all(&data)?.into_iter().find(|w| w.hash16 == h).ok_or("no area with that HASH16")?.id
        }
        None => area.clone(),
    };
    let seed: u32 = seed.parse()?;
    let (pwg1, objects) = instance_server::replay_terrain(&data, &area, seed, &config.travel.skip_objects)?;
    let out = PathBuf::from(out);
    std::fs::create_dir_all(&out)?;
    std::fs::write(out.join(format!("{area}_{seed}.pwg")), pwg1)?;
    let lines: String = objects.iter().map(|o| format!("{} {} {} {} {} {}\n", o.x, o.y, o.reach.reach, o.reach.targeting, o.reach.obstacle.unwrap_or(-1), o.kind)).collect();
    std::fs::write(out.join(format!("{area}_{seed}.objects")), lines)?;
    // The skill ids the server gives the player (`InstanceServer` player state in `lib.rs`): basic
    // actions `0x20000000 | ActionTypes key`, innate skills `index + 1`.
    let basic = |name: &str| poe_data::views::action_type_key(&data, name).ok().flatten().map_or(0, |k| 0x2000_0000 | k as u32);
    let roll_action = poe_data::views::action_type_key(&data, "DodgeRoll")?;
    let mut rolls = Vec::new();
    for (i, id) in config.player.innate_skills.iter().enumerate() {
        if let Some(g) = poe_data::views::GrantedSkill::find(&data, id, 1)?
            && roll_action.is_some()
            && g.action_type == roll_action
        {
            rolls.push((i + 1).to_string());
        }
    }
    println!("{area} move=0x{:x} interact=0x{:x} roll={}", basic("Move"), basic("Interaction"), rolls.join(","));
    Ok(())
}
