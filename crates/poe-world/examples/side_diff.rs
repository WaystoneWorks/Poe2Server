//! Compares the port's server-side collision grid (`Side::Server`, what the instance server
//! builds) with its client-side grid (`Side::Client`, the one checked against the client batch)
//! over the layouts of a client batch.
//!
//! `cargo run --release -p poe-world --example side_diff -- <batch-results.tsv> [--threads N]
//! [--step K] [--out FILE]`
//!
//! Only rows with status `ok` are used, each layout once; `--step K` keeps every K-th layout.
//! Per layout: `area seed topology orientation same|differ cells server_kinds client_kinds`
//! (`cells`: grid cells that differ in any layer; `*_kinds`: the solid, placed, side-flagged object
//! types of that side, `kind:count` comma-separated, `-` for none) to `--out` or stdout. Stderr
//! ends with the counts, the areas with differing layouts and the side-flagged solid types seen.
//! `POE_SIDE_OBJECTS=1` also prints every placed side-flagged object (`OBJ` lines, stderr).

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use poe_data::GameData;
use poe_world::{
    area::Forced,
    doodads::{Doodad, SIDE_CLIENT, SIDE_SERVER, Side},
    generate::CachedFiles,
    rng::TerrainRng,
};

fn kinds(objs: &[&Doodad], side: u8) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    for o in objs.iter().filter(|o| o.side == side && o.solid && o.placed && !o.league && !o.dropped) {
        *m.entry(o.kind.clone()).or_insert(0) += 1;
    }
    m
}

fn fmt(m: &BTreeMap<String, usize>) -> String {
    if m.is_empty() { "-".into() } else { m.iter().map(|(k, n)| format!("{k}:{n}")).collect::<Vec<_>>().join(",") }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args.next().ok_or("usage: side_diff <batch-results.tsv> [--threads N] [--step K] [--out FILE]")?;
    let (mut threads, mut step, mut out_path) = (std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4), 1usize, None);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--threads" => threads = args.next().ok_or("--threads needs a number")?.parse()?,
            "--step" => step = args.next().ok_or("--step needs a number")?.parse()?,
            "--out" => out_path = Some(args.next().ok_or("--out needs a file")?),
            _ => return Err(format!("unknown argument {a}").into()),
        }
    }
    let mut keys = BTreeSet::new();
    for l in std::fs::read_to_string(&path)?.lines() {
        if l.starts_with('#') || l.trim().is_empty() { continue }
        let f: Vec<&str> = l.split('\t').collect();
        if f.len() < 5 || f[4] != "ok" { continue }
        let (Ok(seed), Ok(t), Ok(o)) = (f[1].parse::<u32>(), f[2].parse::<u32>(), f[3].parse::<u8>()) else { continue };
        keys.insert((f[0].to_string(), seed, t, o));
    }
    let keys: Vec<_> = keys.into_iter().step_by(step.max(1)).collect();
    eprintln!("{} layouts", keys.len());
    let gd: &'static GameData = Box::leak(Box::new(GameData::open(Path::new("GameFiles"), Path::new("third_party/dat-schema/schema-poe2.min.json"))?));
    let files: &'static CachedFiles<'static> = Box::leak(Box::new(CachedFiles::new(gd.files())));
    std::panic::set_hook(Box::new(|_| {}));
    let next = AtomicUsize::new(0);
    let rows: Mutex<Vec<Option<(bool, usize, BTreeMap<String, usize>, BTreeMap<String, usize>)>>> = Mutex::new(vec![None; keys.len()]);
    let errors = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads.max(1) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some((area, seed, t, o)) = keys.get(i) else { break };
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut rng = TerrainRng::new(*seed);
                    let g = poe_world::area::generate_with_files(gd, files, area, &mut rng, Some(Forced { topology: Some(*t), orientation: Some(*o) }))?;
                    let objs: Vec<&Doodad> = g.doodads.iter().chain(&g.tile_objects).collect();
                    // `POE_SIDE_OBJECTS=1`: every placed side-flagged object to stderr
                    // (`OBJ area seed topology orientation side x y angle kind`).
                    if std::env::var_os("POE_SIDE_OBJECTS").is_some() {
                        for d in objs.iter().filter(|d| d.side != 0 && d.placed && !d.dropped && !d.league) {
                            eprintln!("OBJ\t{area}\t{seed}\t{t}\t{o}\t{}\t{}\t{}\t{:.4}\t{}", d.side, d.x, d.y, d.angle, d.kind);
                        }
                    }
                    let srv = poe_world::collision::build(&g.map, files, objs.iter().copied(), Side::Server)?.pwg1();
                    let cli = poe_world::collision::build(&g.map, files, objs.iter().copied(), Side::Client)?.pwg1();
                    // Cells differing in any layer: compare nibble by nibble past the 20-byte header.
                    let cells = if srv.len() == cli.len() {
                        srv.iter().zip(&cli).skip(20).map(|(a, b)| ((a ^ b) & 15 != 0) as usize + ((a ^ b) >> 4 != 0) as usize).sum()
                    } else {
                        usize::MAX
                    };
                    Ok::<_, poe_world::generate::GenError>((srv == cli, cells, kinds(&objs, SIDE_SERVER), kinds(&objs, SIDE_CLIENT)))
                }));
                match r {
                    Ok(Ok(v)) => rows.lock().unwrap()[i] = Some(v),
                    _ => { errors.fetch_add(1, Ordering::Relaxed); }
                }
                if i % 5000 == 0 { eprintln!("{i}/{}", keys.len()); }
            });
        }
    });
    let rows = rows.into_inner()?;
    let mut text = String::new();
    let (mut same, mut differ) = (0, 0);
    let mut areas: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    let (mut seen_s, mut seen_c): (BTreeMap<String, usize>, BTreeMap<String, usize>) = Default::default();
    for ((area, seed, t, o), r) in keys.iter().zip(&rows) {
        let Some((eq, cells, s, c)) = r else { continue };
        if *eq { same += 1 } else { differ += 1 }
        let e = areas.entry(area).or_default();
        e.0 += 1;
        e.1 += !*eq as usize;
        for (k, n) in s { *seen_s.entry(k.clone()).or_default() += n }
        for (k, n) in c { *seen_c.entry(k.clone()).or_default() += n }
        text += &format!("{area}\t{seed}\t{t}\t{o}\t{}\t{cells}\t{}\t{}\n", if *eq { "same" } else { "differ" }, fmt(s), fmt(c));
    }
    match out_path {
        Some(p) => std::fs::write(p, text)?,
        None => print!("{text}"),
    }
    eprintln!("same {same}, differ {differ}, errors {}", errors.load(Ordering::Relaxed));
    for (a, (n, d)) in areas.iter().filter(|(_, (_, d))| *d > 0) {
        eprintln!("  {a}: {d}/{n} differ");
    }
    eprintln!("server_only solid placed types: {}", fmt(&seen_s));
    eprintln!("client_only solid placed types: {}", fmt(&seen_c));
    Ok(())
}
