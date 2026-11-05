//! Sets `cfg(game_files)` when the game's data is on this machine: the hub's `GameFiles/`, or
//! `POE_GAME_DIR`. Tests that read it are left out without it, so a plain clone builds and tests.

use std::path::PathBuf;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(game_files)");
    println!("cargo::rerun-if-env-changed=POE_GAME_DIR");
    let hub = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("../../../..");
    let game = std::env::var_os("POE_GAME_DIR").map_or_else(|| hub.join("GameFiles"), PathBuf::from);
    println!("cargo::rerun-if-changed={}", game.display());
    if game.is_dir() {
        println!("cargo::rustc-cfg=game_files");
    }
}
