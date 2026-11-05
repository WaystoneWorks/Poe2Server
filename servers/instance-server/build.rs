//! Puts `git describe` into the build (`POE_SERVER_BUILD`), for terrain mismatch reports. Sets
//! `cfg(game_files)` when the game's data is here (the hub's `GameFiles/` or `POE_GAME_DIR`): tests
//! that read it are ignored without it, so a plain clone builds and tests.

fn main() {
    let describe = std::process::Command::new("git")
        .args(["describe", "--always", "--dirty"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    println!("cargo:rustc-env=POE_SERVER_BUILD={describe}");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../.git/HEAD");

    println!("cargo::rustc-check-cfg=cfg(game_files)");
    println!("cargo::rerun-if-env-changed=POE_GAME_DIR");
    let hub = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("../../../..");
    let game = std::env::var_os("POE_GAME_DIR").map_or_else(|| hub.join("GameFiles"), std::path::PathBuf::from);
    println!("cargo::rerun-if-changed={}", game.display());
    if game.is_dir() {
        println!("cargo::rustc-cfg=game_files");
    }
}
