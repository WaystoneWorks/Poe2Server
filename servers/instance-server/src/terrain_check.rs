//! The terrain check at area entry (docs/protocol/instance.md in the hub).
//!
//! The client reports `TerrainHashes` (tile hash, doodad hash) after it generated the area. The
//! server's generator (`poe-world`) computes both for the same area, seed and layout; when they
//! differ, or the client reports nothing, the player would walk on a grid unlike the client's.
//! Everyone is kicked then ([`Memory::choose`]): back to the instance they came from, else the
//! act's town, else character selection. This module holds the decisions and the texts; the
//! session code in `lib.rs` carries them out.

use std::{
    collections::HashMap,
    io::Write,
    path::Path,
    sync::Mutex,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Two kicks of one character within this time end at character selection (a destination that
/// mismatches too must not loop).
pub const KICK_LOOP_GUARD: Duration = Duration::from_secs(60);

/// How long the server waits for the DLL's `ack export` before it moves the player.
pub const ACK_WAIT: Duration = Duration::from_secs(3);

/// How long a player in the world is left alone before the kick: a client that is handed off
/// while it is still finishing its world entry exits (game test, 2026-11-04).
pub const SETTLE: Duration = Duration::from_secs(3);

/// How long the server waits for the DLL's first datagram once the player is in the world (the
/// DLL sends `hello` every second; position reports start later).
pub const PEER_WAIT: Duration = Duration::from_secs(3);

/// The hashes of a layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hashes {
    pub tile: u32,
    pub doodad: u32,
}

/// Why a session fails the check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Tile,
    Doodad,
    Both,
    /// The client sent no hashes in time (its generation threw `TerrainGenerationException`).
    NoHashes { secs: u64 },
}

impl Reason {
    pub fn text(self) -> String {
        match self {
            Reason::Tile => "tile hash differs".into(),
            Reason::Doodad => "doodad hash differs".into(),
            Reason::Both => "tile and doodad hash differ".into(),
            Reason::NoHashes { secs } => format!("client terrain generation failed (no hashes in {secs} s)"),
        }
    }
}

/// Compares the client's hashes with the server's.
pub fn compare(server: Hashes, client: Hashes) -> Option<Reason> {
    match (server.tile != client.tile, server.doodad != client.doodad) {
        (false, false) => None,
        (true, false) => Some(Reason::Tile),
        (false, true) => Some(Reason::Doodad),
        (true, true) => Some(Reason::Both),
    }
}

/// Where the kicked player goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// The instance the character left last, at the cell it left.
    Previous(Previous),
    /// A new or open instance of this town (`WorldAreas.Id`), at its start.
    Town(String),
    /// Character selection.
    Select,
}

/// Where a character stood when it left an instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Previous {
    pub instance: u32,
    pub area: String,
    pub seed: u32,
    pub cell: (i32, i32),
}

/// What the check knows about the session it judges.
#[derive(Debug, Clone)]
pub struct Mismatch {
    pub account: String,
    pub character: String,
    pub instance: Option<u32>,
    pub area: String,
    pub area_name: String,
    pub seed: u32,
    pub topology: Option<u32>,
    pub orientation: Option<u8>,
    pub client: Option<Hashes>,
    /// What the generator made; `None` when no generation was made (the client never reported).
    pub server: Option<Hashes>,
    pub reason: Reason,
    /// The server's expectation was changed by `[dev] fake_terrain_hash`.
    pub fake: bool,
    pub server_build: String,
    pub client_build: String,
}

fn hex(h: Option<u32>) -> String {
    h.map_or("none".into(), |h| format!("0x{h:08x}"))
}

/// A hash in the report line (no `0x`, as in the client log).
fn bare(h: Option<u32>) -> String {
    h.map_or("none".into(), |h| format!("{h:08x}"))
}

fn opt<T: ToString>(v: Option<T>) -> String {
    v.map_or("?".into(), |v| v.to_string())
}

/// A field value on one line, without tabs.
fn clean(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

impl Mismatch {
    /// The reason, marked when the dev switch faked the server's hash.
    pub fn reason_text(&self) -> String {
        match self.fake {
            true => format!("{} (dev fake_terrain_hash)", self.reason.text()),
            false => self.reason.text(),
        }
    }

    fn tile_cmp(&self) -> (Option<u32>, Option<u32>) {
        (self.client.map(|c| c.tile), self.server.map(|s| s.tile))
    }

    /// The one-line report the popup copies.
    pub fn report(&self) -> String {
        let (ct, st) = self.tile_cmp();
        format!(
            "terrain-mismatch build={} server={} area={} seed={} topo={} orient={} tile={}/{} doodad={}/{} reason={}",
            self.client_build,
            self.server_build,
            self.area,
            self.seed,
            opt(self.topology),
            opt(self.orientation),
            bare(ct),
            bare(st),
            bare(self.client.map(|c| c.doodad)),
            bare(self.server.map(|s| s.doodad)),
            self.reason_text().replace(' ', "_"),
        )
    }

    /// The debug-link `alert` block (docs/protocol/side-channel.md): sent in one datagram.
    pub fn alert(&self, dest: &str) -> String {
        let (ct, st) = self.tile_cmp();
        let area = if self.area_name.is_empty() { self.area.clone() } else { format!("{} ({})", self.area, self.area_name) };
        format!(
            "alert\tterrain_mismatch\narea\t{}\nseed\t{}\nlayout\ttopology {}, orientation {}\ntile\tclient {}  server {}\ndoodad\tclient {}  server {}\nreason\t{}\ndest\t{}\nreport\t{}\nend\n",
            clean(&area),
            self.seed,
            opt(self.topology),
            opt(self.orientation),
            hex(ct),
            hex(st),
            hex(self.client.map(|c| c.doodad)),
            hex(self.server.map(|s| s.doodad)),
            clean(&self.reason_text()),
            clean(dest),
            clean(&self.report()),
        )
    }

    /// The row of `terrain_mismatches.tsv` ([`TSV_HEADER`]).
    pub fn tsv_row(&self, dest: &str) -> String {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let cols = [
            now.to_string(),
            clean(&self.account),
            clean(&self.character),
            opt(self.instance),
            clean(&self.area),
            clean(&self.area_name),
            self.seed.to_string(),
            opt(self.topology),
            opt(self.orientation),
            self.client.map_or("none".into(), |c| c.tile.to_string()),
            self.server.map_or("none".into(), |s| s.tile.to_string()),
            self.client.map_or("none".into(), |c| c.doodad.to_string()),
            self.server.map_or("none".into(), |s| s.doodad.to_string()),
            clean(&self.reason_text()),
            clean(dest),
            clean(&self.server_build),
            clean(&self.client_build),
        ];
        cols.join("\t")
    }
}

/// Columns of `terrain_mismatches.tsv`; `cargo xtask terrain-matrix` reads `area`, `seed`,
/// `topology`, `orientation` and the hashes.
pub const TSV_HEADER: &str = "time\taccount\tcharacter\tinstance\tarea\tarea_name\tseed\ttopology\torientation\tclient_tile\tserver_tile\tclient_doodad\tserver_doodad\treason\tdest\tserver_build\tclient_build";

/// Appends a mismatch row to `path` (a header first when the file is new). Empty path: off.
pub fn append_row(path: &Path, row: &str) -> std::io::Result<()> {
    if path.as_os_str().is_empty() {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let fresh = !path.exists();
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    if fresh {
        writeln!(f, "{TSV_HEADER}")?;
    }
    writeln!(f, "{row}")
}

/// Per character: the place it left last, and when it was last kicked.
#[derive(Default)]
pub struct Memory {
    previous: Mutex<HashMap<u32, Previous>>,
    kicked: Mutex<HashMap<u32, Instant>>,
}

impl Memory {
    /// Notes the place a character left (every session that entered the world, however it
    /// ended).
    pub fn remember(&self, character: u32, at: Previous) {
        self.previous.lock().unwrap().insert(character, at);
    }

    /// The destination of a kicked character, and the kick is noted. `current` is the instance
    /// that failed (never a destination), `current_area` its area, `exists` tells whether an
    /// instance is still open, `town` the town of the area's act.
    pub fn choose(&self, character: u32, current: Option<u32>, current_area: &str, exists: impl Fn(u32) -> bool, town: Option<&str>) -> Destination {
        let now = Instant::now();
        let again = self.kicked.lock().unwrap().insert(character, now).is_some_and(|t| now.duration_since(t) < KICK_LOOP_GUARD);
        if again {
            return Destination::Select;
        }
        if let Some(p) = self.previous.lock().unwrap().get(&character)
            && Some(p.instance) != current
            && exists(p.instance)
        {
            return Destination::Previous(p.clone());
        }
        match town {
            Some(t) if t != current_area => Destination::Town(t.to_string()),
            _ => Destination::Select,
        }
    }
}

use poe_cluster::Ticket;
use poe_data::views::WorldArea;
use poe_net::Connection;
use tracing::{info, warn};

use crate::InstanceServer;

impl InstanceServer {
    /// The facts of a failed check for the session of `ticket`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn mismatch(&self, ticket: &Ticket, instance: Option<u32>, seed: u32, layout: Option<poe_world::area::Forced>, client: Option<Hashes>, server: Option<Hashes>, reason: Reason) -> Mismatch {
        let area_name = WorldArea::find(&self.data, &ticket.area).ok().flatten().map(|a| a.name).unwrap_or_default();
        let account = self.store.account_by_id(ticket.account_id).ok().flatten().map(|a| a.name).unwrap_or_else(|| format!("account {}", ticket.account_id));
        Mismatch {
            account,
            character: ticket.character.name.clone(),
            instance,
            area: ticket.area.clone(),
            area_name,
            seed,
            topology: layout.and_then(|l| l.topology),
            orientation: layout.and_then(|l| l.orientation),
            client,
            server,
            reason,
            fake: false,
            server_build: env!("POE_SERVER_BUILD").to_string(),
            client_build: self.config.game.version.clone(),
        }
    }

    /// Ends the session of a client whose terrain does not match: logs and records it, tells the
    /// DLL (popup, layout export) and moves the player to its [`Destination`]. The player has
    /// not entered the world yet, so there is nothing to leave.
    pub(crate) async fn kick_for_terrain(&self, conn: &mut Connection, ticket: &Ticket, side: u64, m: Mismatch, in_world: bool) -> anyhow::Result<()> {
        let area = WorldArea::find(&self.data, &ticket.area)?;
        let town = match &area {
            Some(a) => a.town(&self.data)?.map(|t| t.id),
            None => None,
        };
        let dest = self.terrain_memory.choose(ticket.character.id, m.instance, &ticket.area, |i| self.instances.get(i).is_some(), town.as_deref());
        let dest_text = match &dest {
            Destination::Previous(p) => {
                let name = WorldArea::find(&self.data, &p.area).ok().flatten().map(|a| a.name).unwrap_or_default();
                format!("previous instance {} ({} {name})", p.instance, p.area)
            }
            Destination::Town(id) => {
                let name = WorldArea::find(&self.data, id).ok().flatten().map(|a| a.name).unwrap_or_default();
                format!("town {id} ({name})")
            }
            Destination::Select => "character select".to_string(),
        };
        warn!(
            account = %m.account,
            character = %m.character,
            area = %m.area,
            seed = m.seed,
            reason = %m.reason.text(),
            dest = %dest_text,
            "terrain mismatch: {}",
            m.report()
        );
        if let Err(e) = append_row(&self.config.terrain.mismatch_log, &m.tsv_row(&dest_text)) {
            warn!(path = %self.config.terrain.mismatch_log.display(), "terrain mismatch log: {e}");
        }
        // The DLL exports the client's layout while the client still has the area loaded, then
        // acknowledges. In the world its position reports (the first datagram tells the server
        // where to send) start within a moment; without a DLL a chat line tells the player.
        let mut dll = false;
        if in_world {
            tokio::time::sleep(SETTLE).await;
        }
        if self.config.dev.debug_link {
            let end = tokio::time::Instant::now() + if in_world { PEER_WAIT } else { Duration::ZERO };
            loop {
                dll = self.reports.has_peer(side);
                if dll || tokio::time::Instant::now() >= end {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        if dll {
            self.reports.send_debug(side, &m.alert(&dest_text));
            let end = tokio::time::Instant::now() + ACK_WAIT;
            'ack: while tokio::time::Instant::now() < end {
                for line in self.reports.take_commands(side) {
                    if line.starts_with("ack export") {
                        info!("DLL: {line}");
                        break 'ack;
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        } else if in_world {
            info!("terrain mismatch: no DLL on the debug link, telling the player in chat");
            let text = format!("Your terrain does not match the server's ({}). You are being moved to: {dest_text}.", m.reason_text());
            let line = crate::live::ChatOut { sender: crate::SERVER_NAME.into(), text };
            conn.send(&self.protocol.instance.encode(&crate::chat_line(&line))?).await?;
            tokio::time::sleep(Duration::from_secs(2)).await;
        } else {
            info!("terrain mismatch: no DLL on the debug link, moving the player at once");
        }
        let moved = match &dest {
            Destination::Previous(p) => self.hand_off_at(ticket.account_id, ticket.character.clone(), p.instance, Some((p.seed, p.cell.0, p.cell.1))),
            Destination::Town(id) => {
                let id = match self.instances.find(id) {
                    Some(i) => i,
                    None => self.instances.create(poe_cluster::InstanceSpec { area: id.clone(), seed: WorldArea::find(&self.data, id)?.and_then(|a| a.fixed_seed()), ..Default::default() }, true),
                };
                self.hand_off(ticket.account_id, ticket.character.clone(), id)
            }
            Destination::Select => Err(anyhow::anyhow!("character select")),
        };
        if let Err(e) = moved {
            if !matches!(dest, Destination::Select) {
                warn!("terrain mismatch: could not hand the player off ({e:#}): sending it to character select");
            }
            let resume = self.tickets.issue_resume(ticket.account_id);
            let reply = poe_proto::instance::ExitSession { unk_10: 0, unk_20: resume.account_id, unk_18: resume.token };
            conn.send(&self.protocol.instance.encode(&reply)?).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Hashes = Hashes { tile: 1, doodad: 2 };

    fn mismatch() -> Mismatch {
        Mismatch {
            account: "acct".into(),
            character: "Char".into(),
            instance: Some(9),
            area: "G1_2".into(),
            area_name: "Clearfell".into(),
            seed: 123456789,
            topology: Some(3),
            orientation: Some(1),
            client: Some(Hashes { tile: 0x1a2b3c4d, doodad: 0x11111111 }),
            server: Some(Hashes { tile: 0x5e6f7081, doodad: 0x11111111 }),
            reason: Reason::Tile,
            fake: false,
            server_build: "abc123".into(),
            client_build: "0.5.5e.hotfix1".into(),
        }
    }

    #[test]
    fn compare_names_what_differs() {
        assert_eq!(compare(A, A), None);
        assert_eq!(compare(A, Hashes { tile: 9, ..A }), Some(Reason::Tile));
        assert_eq!(compare(A, Hashes { doodad: 9, ..A }), Some(Reason::Doodad));
        assert_eq!(compare(A, Hashes { tile: 8, doodad: 9 }), Some(Reason::Both));
    }

    #[test]
    fn alert_block_has_the_documented_lines() {
        let alert = mismatch().alert("previous instance 7 (G1_1 Riverbank)");
        let keys: Vec<&str> = alert.lines().map(|l| l.split('\t').next().unwrap()).collect();
        assert_eq!(keys, ["alert", "area", "seed", "layout", "tile", "doodad", "reason", "dest", "report", "end"]);
        assert!(alert.contains("area\tG1_2 (Clearfell)\n"));
        assert!(alert.contains("tile\tclient 0x1a2b3c4d  server 0x5e6f7081\n"));
        assert!(alert.contains("doodad\tclient 0x11111111  server 0x11111111\n"));
        assert!(alert.contains("report\tterrain-mismatch build=0.5.5e.hotfix1 server=abc123 area=G1_2 seed=123456789 topo=3 orient=1 tile=1a2b3c4d/5e6f7081 doodad=11111111/11111111"));
    }

    #[test]
    fn missing_hashes_are_named() {
        let mut m = mismatch();
        m.client = None;
        m.reason = Reason::NoHashes { secs: 30 };
        assert!(m.alert("character select").contains("tile\tclient none  server 0x5e6f7081"));
        assert!(m.tsv_row("x").contains("\tnone\t"));
        assert_eq!(m.tsv_row("x").split('\t').count(), TSV_HEADER.split('\t').count());
    }

    fn prev(instance: u32) -> Previous {
        Previous { instance, area: "G1_1".into(), seed: 5, cell: (10, 20) }
    }

    #[test]
    fn goes_to_the_previous_instance_when_it_is_open() {
        let m = Memory::default();
        m.remember(1, prev(7));
        assert_eq!(m.choose(1, Some(9), "G1_2", |i| i == 7, Some("G1_town")), Destination::Previous(prev(7)));
    }

    #[test]
    fn goes_to_the_town_when_the_previous_instance_closed_or_is_unknown() {
        let m = Memory::default();
        m.remember(1, prev(7));
        assert_eq!(m.choose(1, Some(9), "G1_2", |_| false, Some("G1_town")), Destination::Town("G1_town".into()));
        assert_eq!(m.choose(2, Some(9), "G1_2", |_| true, Some("G1_town")), Destination::Town("G1_town".into()));
    }

    #[test]
    fn the_failed_instance_is_never_the_previous_one() {
        let m = Memory::default();
        m.remember(1, prev(9));
        assert_eq!(m.choose(1, Some(9), "G1_2", |_| true, Some("G1_town")), Destination::Town("G1_town".into()));
    }

    #[test]
    fn a_failing_town_or_a_second_kick_goes_to_character_select() {
        let m = Memory::default();
        assert_eq!(m.choose(1, Some(9), "G1_town", |_| true, Some("G1_town")), Destination::Select);
        assert_eq!(m.choose(2, Some(9), "G1_2", |_| true, None), Destination::Select);
        let m = Memory::default();
        m.remember(1, prev(7));
        assert!(matches!(m.choose(1, Some(9), "G1_2", |_| true, Some("G1_town")), Destination::Previous(_)));
        assert_eq!(m.choose(1, Some(10), "G1_3", |_| true, Some("G1_town")), Destination::Select);
    }
}
