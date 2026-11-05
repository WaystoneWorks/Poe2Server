//! Running [`crate::commands`] for a session (chat `!` commands and the overlay).

use poe_cluster::{InstanceSpec, Ticket};
use poe_data::views::WorldArea;
use poe_mod_host::CommandAction;
use tracing::{info, warn};

use crate::{
    InstanceServer,
    commands::{self, Parsed},
    live::ChatOut,
    player,
};

/// The session a command runs in.
pub struct SessionCtx<'a> {
    /// The session's travel objects and unlock state.
    pub travel: &'a crate::travel::Travel,
    pub ticket: &'a Ticket,
    pub account: String,
    pub instance: Option<u32>,
    pub seed: u32,
    /// The client's tile hash for this layout.
    pub tile_hash: u32,
    pub port: u16,
    /// Side-channel session id (position reports, debug link; `sync::ClientReports`).
    pub side: u64,
}

/// What a command produced.
#[derive(Default)]
pub struct Outcome {
    pub text: String,
    pub ok: bool,
    /// Raw messages for this session's client.
    pub raw: Vec<u8>,
    /// End this session after the reply (the client is being handed to another instance).
    pub leave: bool,
}

impl Outcome {
    pub(crate) fn ok(text: impl Into<String>) -> Self {
        Self { text: text.into(), ok: true, ..Default::default() }
    }

    pub(crate) fn err(text: impl Into<String>) -> Self {
        Self { text: text.into(), ok: false, ..Default::default() }
    }
}

/// How many aliases of mod commands may call each other.
const MAX_ALIAS_DEPTH: u8 = 4;

/// The name server messages are sent under.
pub const SERVER_NAME: &str = "<Server>";

/// The frame length (ms) for a time scale: at most the tick (the client plays faster only with
/// frames queued, which adds latency) and at least 1 ms. `None` when the factor is out of range.
fn frame_ms_for(factor: f64, tick_ms: u32) -> Option<u32> {
    let ms = (factor * f64::from(tick_ms)).round();
    (factor > 0.0 && factor <= 1.0 && ms >= 1.0).then_some(ms as u32)
}

impl InstanceServer {
    /// Parses and runs a command line (without the chat prefix).
    pub(crate) fn run_command(&self, ctx: &SessionCtx<'_>, player: &mut player::Player, line: &str) -> Outcome {
        self.run_command_depth(ctx, player, line, 0)
    }

    /// Finds a line's command, checks the caller's role and the arguments, and turns names into
    /// ids. The role is checked at the time of the command (a demoted admin loses it at once),
    /// before the arguments, so messages name only what the caller may see.
    fn parse_checked(&self, actor: &str, line: &str) -> Result<Parsed, String> {
        let (spec, rest) = commands::locate(line).map_err(|miss| miss.text(self.is_admin(actor)))?;
        if spec.is_admin() && !self.is_admin(actor) {
            return Err(format!("{} is for admins only", spec.name()));
        }
        let mut parsed = commands::parse_args(spec, rest)?;
        commands::resolve(&mut parsed, self)?;
        Ok(parsed)
    }

    /// [`run_command`](Self::run_command) for an alias of a mod: `depth` counts the aliases
    /// followed so far.
    fn run_command_depth(&self, ctx: &SessionCtx<'_>, player: &mut player::Player, line: &str, depth: u8) -> Outcome {
        let parsed = match self.parse_checked(&ctx.account, line) {
            Ok(p) => p,
            Err(e) => return Outcome::err(e),
        };
        let out = match commands::extra(parsed.spec) {
            Some(extra) => self.execute_mod(ctx, player, &parsed, extra, depth),
            None => self.execute(ctx, player, &parsed),
        };
        self.finish(&ctx.account, &parsed, line, out)
    }

    /// A command that a mod added: a fixed reply, an alias of another command line, or the
    /// mod's own code (run by the instance actor; the answer arrives as the chat lines the mod
    /// submits, so this outcome has no text).
    fn execute_mod(
        &self,
        ctx: &SessionCtx<'_>,
        player: &mut player::Player,
        p: &Parsed,
        extra: &commands::Extra,
        depth: u8,
    ) -> anyhow::Result<Outcome> {
        let args = p.get("args").unwrap_or_default();
        let fill = |text: &str| commands::fill(text, &ctx.ticket.character.name, args);
        Ok(match &extra.action {
            CommandAction::Reply(text) => Outcome::ok(fill(text)),
            CommandAction::Alias(line) => {
                if depth >= MAX_ALIAS_DEPTH {
                    return Ok(Outcome::err(format!("{}: aliases nest too deep", p.spec.name())));
                }
                // The target command is checked like a typed one (its role, its arguments).
                return Ok(self.run_command_depth(ctx, player, fill(line).trim(), depth + 1));
            }
            CommandAction::Native => {
                if player.mod_command(&extra.mod_id, p.spec.path[0], args) {
                    Outcome::ok("")
                } else {
                    Outcome::err("the instance is not running")
                }
            }
        })
    }

    /// Runs a command that needs no game session of its own (the web API's `AdminControl`):
    /// the same registry, role check, log and audit as chat commands. `account` is the label
    /// logged and audited.
    pub(crate) fn run_global(&self, account: &str, actor: &str, line: &str) -> Outcome {
        let parsed = match self.parse_checked(actor, line) {
            Ok(p) => p,
            Err(e) => return Outcome::err(e),
        };
        let out = self.execute_global(actor, &parsed).unwrap_or_else(|| {
            Ok(Outcome::err(format!("{} needs your own game session", parsed.spec.name())))
        });
        self.finish(account, &parsed, line, out)
    }

    /// Logs a command's result and appends admin commands to the audit file. Passwords never
    /// reach either.
    pub(crate) fn finish(&self, account: &str, parsed: &Parsed, line: &str, result: anyhow::Result<Outcome>) -> Outcome {
        let shown = match parsed.spec.path {
            ["account"] => poe_accounts::redact(parsed.get("action").unwrap_or_default(), line),
            _ => line.to_string(),
        };
        let out = match result {
            Ok(o) => o,
            Err(e) => Outcome::err(format!("{e:#}")),
        };
        let flat = out.text.replace('\n', " | ");
        info!(account = %account, command = %shown, ok = out.ok, result = %flat, "command");
        if parsed.spec.is_admin() {
            self.audit(account, &shown, &flat);
        }
        out
    }

    /// The commands that work without a game session. `None`: the command needs one.
    pub(crate) fn execute_global(&self, actor: &str, p: &Parsed) -> Option<anyhow::Result<Outcome>> {
        let prefix = &self.config.commands.prefix;
        Some(Ok(match p.spec.path {
            ["help"] => {
                let (query, page) = commands::split_page(p.get("search"));
                let lines = commands::help_lines(query, self.is_admin(actor), prefix);
                let next = format!("{prefix}help{}", query.map_or(String::new(), |q| format!(" {q}")));
                Outcome::ok(commands::paged(&lines, page, &next))
            }
            ["uptime"] => {
                let secs = self.started.elapsed().as_secs();
                Outcome::ok(format!(
                    "up {}d {}h {}m; server {}, client {}",
                    secs / 86400,
                    secs / 3600 % 24,
                    secs / 60 % 60,
                    env!("CARGO_PKG_VERSION"),
                    self.config.game.version
                ))
            }
            ["who"] => {
                let rows: Vec<String> = self
                    .live
                    .online()
                    .into_iter()
                    .map(|o| format!("{} ({}) level {} {} in {} #{}", o.character, o.account, o.level, o.league, o.area, o.instance.map_or("-".into(), |i| i.to_string())))
                    .collect();
                Outcome::ok(format!("{} online: {}", rows.len(), rows.join("; ")))
            }
            ["player", "kick"] => {
                let who = p.get("player").unwrap_or_default();
                let reason = p.get("reason");
                if let Some(reason) = reason {
                    self.live.tell(who, ChatOut { sender: SERVER_NAME.into(), text: format!("You were kicked: {reason}") });
                }
                if self.live.kick(who) {
                    Outcome::ok(match reason {
                        Some(r) => format!("kicked {who}: {r}"),
                        None => format!("kicked {who}"),
                    })
                } else {
                    Outcome::err(format!("{who} is not online"))
                }
            }
            ["player", "mute"] => {
                let who = p.get("player").unwrap_or_default();
                let duration = p.duration("duration");
                self.live.mute(who, duration);
                Outcome::ok(match duration {
                    Some(d) => format!("muted {who} for {}", commands::show_duration(d)),
                    None => format!("muted {who}"),
                })
            }
            ["player", "unmute"] => {
                let who = p.get("player").unwrap_or_default();
                if self.live.unmute(who) { Outcome::ok(format!("unmuted {who}")) } else { Outcome::err(format!("{who} was not muted")) }
            }
            ["announce"] => {
                let text = p.get("message").unwrap_or_default().to_string();
                let n = self.live.broadcast(ChatOut { sender: SERVER_NAME.into(), text }, |_| true);
                Outcome::ok(format!("sent to {n} players"))
            }
            ["unlock"] | ["lock"] => {
                let unlock = p.spec.path == ["unlock"];
                let name = p.get("character").unwrap_or_default();
                let what = p.get("what").unwrap_or("all");
                let found = self.live.online().into_iter().find(|o| o.character.eq_ignore_ascii_case(name));
                let Some(online) = found else {
                    return Some(Ok(Outcome::err(format!("{name} is not online"))));
                };
                let id = match self.store.characters(online.account_id) {
                    Ok(list) => list.into_iter().find(|c| c.name.eq_ignore_ascii_case(name)).map(|c| c.id),
                    Err(e) => return Some(Err(e.into())),
                };
                let Some(id) = id else {
                    return Some(Ok(Outcome::err(format!("{name} is not in the database"))));
                };
                match self.unlock_command(id, unlock, what, p.get("value")) {
                    Ok(n) => Outcome::ok(format!("{} {n} waypoints for {name}", if unlock { "unlocked" } else { "locked" })),
                    Err(e) => Outcome::err(e.to_string()),
                }
            }
            ["area", "list"] => {
                let (query, page) = commands::split_page(p.get("search"));
                match self.area_lines(query) {
                    Ok(lines) if lines.is_empty() => Outcome::err(format!("no area matches {:?}", query.unwrap_or_default())),
                    Ok(lines) => {
                        let next = format!("{prefix}area list{}", query.map_or(String::new(), |q| format!(" {q}")));
                        Outcome::ok(commands::paged(&lines, page, &next))
                    }
                    Err(e) => return Some(Err(e)),
                }
            }
            ["instance", "list"] => {
                let list = self.instances.list();
                let rows: Vec<String> = list
                    .iter()
                    .map(|i| {
                        // The player list is written by the instance's actor.
                        format!(
                            "#{} {} seed {} topo {} orient {} players {} [{}]",
                            i.id,
                            i.spec.area,
                            i.spec.seed.map_or("-".into(), |s| s.to_string()),
                            i.spec.topology.map_or("natural".into(), |t| t.to_string()),
                            i.spec.orientation.map_or("natural".into(), |o| o.to_string()),
                            self.actors.players(i.id).unwrap_or(i.players.len()),
                            i.players.join(", ")
                        )
                    })
                    .collect();
                Outcome::ok(format!("{} instances: {}", rows.len(), rows.join("; ")))
            }
            ["instance", "new"] => match self.new_instance(p) {
                Ok(id) => Outcome::ok(format!("created instance #{id}")),
                Err(e) => return Some(Err(e)),
            },
            ["instance", "close"] => {
                let id = p.int("id").unwrap_or(0) as u32;
                if self.instances.close(id) { Outcome::ok(format!("closed instance #{id}")) } else { Outcome::err(format!("no instance #{id}")) }
            }
            ["account"] => match poe_accounts::run_account_command(
                &*self.accounts,
                Some(actor),
                p.get("action").unwrap_or_default(),
                p.get("name"),
                p.get("value"),
            ) {
                Ok(done) => {
                    if let Some(id) = done.kick_account {
                        // Ends its login connection and its game session.
                        self.sessions.supersede(id);
                    }
                    Outcome::ok(done.text)
                }
                Err(e) => Outcome::err(e),
            },
            ["league", ..] => self.league_command(actor, p),
            _ => return None,
        }))
    }

    /// `area list`: one line per area whose id or name contains the search word.
    fn area_lines(&self, query: Option<&str>) -> anyhow::Result<Vec<String>> {
        let q = query.unwrap_or_default().to_lowercase();
        Ok(WorldArea::all(&self.data)?
            .into_iter()
            .filter(|a| a.id.to_lowercase().contains(&q) || a.name.to_lowercase().contains(&q))
            .map(|a| format!("{}  {}  act {} level {}", a.id, a.name, a.act, a.level))
            .collect())
    }

    /// `league list` and `league end <league> <target>` (admins; the web admin does the rest).
    fn league_command(&self, actor: &str, p: &Parsed) -> Outcome {
        let store = &*self.leagues;
        match p.spec.path {
            ["league", "list"] => match (store.leagues(), store.character_counts()) {
                (Ok(rows), Ok(counts)) => {
                    let rows: Vec<String> = rows
                        .iter()
                        .map(|l| format!("{} [{}] {} chars", l.id, l.state.as_str(), counts.get(&l.id).copied().unwrap_or(0)))
                        .collect();
                    Outcome::ok(format!("{} leagues: {}", rows.len(), rows.join("; ")))
                }
                (Err(e), _) | (_, Err(e)) => Outcome::err(format!("league list failed: {e}")),
            },
            ["league", "end"] => {
                let (Some(league), Some(target)) = (p.get("league"), p.get("target")) else {
                    return Outcome::err(format!("usage: {}", commands::usage(p.spec)));
                };
                let family = match store.league(league) {
                    Ok(Some(l)) => l.family,
                    Ok(None) => return Outcome::err(format!("no league {league:?}")),
                    Err(e) => return Outcome::err(format!("league lookup failed: {e}")),
                };
                match poe_web_api::leagues::end_family(store, &family, target, actor) {
                    Ok(Ok(moves)) => Outcome::ok(format!("ended {family}: {} characters moved to {target}", moves.len())),
                    Ok(Err(refused)) => Outcome::err(refused.to_string()),
                    Err(e) => Outcome::err(format!("league end failed: {e}")),
                }
            }
            other => Outcome::err(format!("unknown league command {other:?}")),
        }
    }

    fn execute(&self, ctx: &SessionCtx<'_>, player: &mut player::Player, p: &Parsed) -> anyhow::Result<Outcome> {
        if let Some(done) = self.execute_global(&ctx.account, p) {
            return done;
        }
        let name = &ctx.ticket.character.name;
        Ok(match p.spec.path {
            ["where"] => {
                let (x, y) = player.cell();
                let players = ctx.instance.and_then(|i| self.actors.players(i)).unwrap_or(0);
                Outcome::ok(format!(
                    "instance {} ({players} players) area {} league {} seed {} cell {x} {y}",
                    ctx.instance.map_or("-".to_string(), |i| i.to_string()),
                    ctx.ticket.area,
                    ctx.ticket.character.league,
                    ctx.seed
                ))
            }
            ["seed"] => {
                let spec = ctx.instance.and_then(|i| self.instances.get(i)).map(|i| i.spec);
                let show = |v: Option<String>| v.unwrap_or_else(|| "natural".into());
                Outcome::ok(format!(
                    "seed {} topology {} orientation {} in {}",
                    ctx.seed,
                    show(spec.as_ref().and_then(|s| s.topology).map(|t| t.to_string())),
                    show(spec.as_ref().and_then(|s| s.orientation).map(|o| o.to_string())),
                    ctx.ticket.area
                ))
            }
            ["stuck"] => {
                player.unstick();
                Outcome::ok("moving you to the nearest walkable cell")
            }
            ["instance", "transfer"] => {
                let id = self.new_instance(p)?;
                self.move_to(ctx, id)?;
                Outcome { leave: true, ..Outcome::ok(format!("moving to new instance #{id}")) }
            }
            ["instance", "join"] => {
                let id = p.int("id").unwrap_or(0) as u32;
                if self.instances.get(id).is_none() {
                    return Ok(Outcome::err(format!("no instance #{id}")));
                }
                if Some(id) == ctx.instance {
                    return Ok(Outcome::err("you are already in it"));
                }
                self.move_to(ctx, id)?;
                Outcome { leave: true, ..Outcome::ok(format!("moving to instance #{id}")) }
            }
            ["instance", "reset"] => {
                let Some(area) = WorldArea::find(&self.data, &ctx.ticket.area)? else {
                    anyhow::bail!("no area {} in WorldAreas", ctx.ticket.area);
                };
                let spec = InstanceSpec { seed: area.fixed_seed(), area: area.id, topology: None, orientation: None };
                let id = self.instances.create(spec, false);
                self.move_to(ctx, id)?;
                Outcome { leave: true, ..Outcome::ok(format!("moving to fresh instance #{id}")) }
            }
            ["tp"] => return self.teleport_command(ctx, player, p),
            ["waypoint", "unlock"] => {
                let n = self.unlock_command(ctx.ticket.character.id, true, "all", None)?;
                Outcome::ok(format!("{n} waypoints unlocked"))
            }
            ["checkpoint", "unlock"] => {
                let n = self.unlock_instance_checkpoints(ctx)?;
                Outcome::ok(format!("{n} checkpoints of this instance unlocked"))
            }
            ["travel", "status"] => {
                let id = ctx.ticket.character.id;
                let (waypoints, checkpoints) = (self.store.waypoints(id)?, self.store.checkpoints(id, &ctx.ticket.area, ctx.seed)?);
                let respawn = self.store.respawn(id)?.map_or("none".to_string(), |r| format!("{} {} {} {}", r.area, r.seed, r.x, r.y));
                Outcome::ok(format!("waypoints {} checkpoints here {} of {} objects, respawn {respawn}", waypoints.len(), checkpoints.len(), ctx.travel.objects.len()))
            }
            ["summon"] => return self.summon(ctx, player, p),
            ["time", "scale"] => return self.time_scale(ctx, player, p),
            ["speed"] => {
                let increased = p.int("increased").map(|v| v as i32);
                let action = p.int("action").map(|v| v as i32);
                if increased.is_none() && action.is_none() {
                    return Ok(Outcome::err(format!("usage: {}", commands::usage(p.spec))));
                }
                player.set_speed_stats(increased, action);
                Outcome::ok(format!("speed: increased {increased:?} %, action speed {action:?} %"))
            }
            ["terrain", "compare"] => {
                self.generate_terrain(&ctx.ticket.area, ctx.seed, ctx.tile_hash, ctx.instance);
                Outcome::ok("generating on the server: the result is in the server log")
            }
            ["research", "area_info"] => {
                let args = format!("{} {}", p.get("area").unwrap_or_default(), p.get("seed").unwrap_or("1"));
                let mut o = Outcome::ok("sent a second AreaInfo");
                o.raw = self.area_info_command(&args, ctx.ticket)?;
                o
            }
            other => {
                warn!(command = ?other, name, "command without an implementation");
                Outcome::err(format!("{} is not implemented", other.join(" ")))
            }
        })
    }

    /// `tp <x> <y>` in this instance, `tp <player>` to their cell (and their instance).
    fn teleport_command(&self, ctx: &SessionCtx<'_>, player: &mut player::Player, p: &Parsed) -> anyhow::Result<Outcome> {
        let target = p.get("x|player").unwrap_or_default();
        if let Some(y) = p.int("y") {
            let Ok(x) = target.parse::<i32>() else {
                return Ok(Outcome::err(format!("bad <x>: {target:?}; usage: {}", commands::usage(p.spec))));
            };
            player.teleport((x, y as i32));
            return Ok(Outcome::ok(format!("teleported to {x} {y}")));
        }
        let Some(other) = self.live.online().into_iter().find(|o| o.character.eq_ignore_ascii_case(target)) else {
            return Ok(Outcome::err(format!("{target} is not online")));
        };
        if other.instance == ctx.instance {
            let (x, y) = other.cell;
            player.teleport((x, y));
            return Ok(Outcome::ok(format!("teleported to {} at {x} {y}", other.character)));
        }
        let Some(id) = other.instance else {
            return Ok(Outcome::err(format!("{} is in no instance", other.character)));
        };
        let place = self.instances.get(id).and_then(|i| i.spec.seed).map(|seed| (seed, other.cell.0, other.cell.1));
        self.hand_off_at(ctx.ticket.account_id, ctx.ticket.character.clone(), id, place)?;
        Ok(Outcome { leave: true, ..Outcome::ok(format!("moving to {} in instance #{id}", other.character)) })
    }

    /// `summon <player>`: onto your cell, through a hand-off when they are in another instance.
    fn summon(&self, ctx: &SessionCtx<'_>, player: &mut player::Player, p: &Parsed) -> anyhow::Result<Outcome> {
        let target = p.get("player").unwrap_or_default();
        let Some(other) = self.live.online().into_iter().find(|o| o.character.eq_ignore_ascii_case(target)) else {
            return Ok(Outcome::err(format!("{target} is not online")));
        };
        if other.character == ctx.ticket.character.name {
            return Ok(Outcome::err("that is you"));
        }
        let (x, y) = player.cell();
        if other.instance == ctx.instance {
            player.teleport_session(other.session, (x, y));
            return Ok(Outcome::ok(format!("{} moved to {x} {y}", other.character)));
        }
        let Some(id) = ctx.instance else {
            return Ok(Outcome::err("you are in no instance"));
        };
        self.transfer_character(other.account_id, &other.character, id, Some((ctx.seed, x, y)))?;
        Ok(Outcome::ok(format!("moving {} to instance #{id}", other.character)))
    }

    /// `time scale [factor]`: the frame length of the whole instance (docs/re/time-scale.md).
    fn time_scale(&self, ctx: &SessionCtx<'_>, player: &mut player::Player, p: &Parsed) -> anyhow::Result<Outcome> {
        let tick = self.config.instance.tick_ms.max(1) as u32;
        let Some(factor) = p.float("factor") else {
            let ms = player.frame_ms();
            return Ok(Outcome::ok(format!("time scale {:.2} (frame length {ms} ms of {tick} ms)", f64::from(ms) / f64::from(tick))));
        };
        let Some(ms) = frame_ms_for(factor, tick) else {
            return Ok(Outcome::err(format!("factor must be from {:.2} to 1 (steps of 1/{tick})", 1.0 / f64::from(tick))));
        };
        player.set_frame_ms(ms);
        info!(instance = ?ctx.instance, frame_ms = ms, "time scale");
        Ok(Outcome::ok(format!("time scale {:.2} for this instance (frame length {ms} ms)", f64::from(ms) / f64::from(tick))))
    }

    /// Creates an instance from `area [seed] [topology] [orientation]`, checking the layout
    /// choice against the area's topologies.
    pub(crate) fn new_instance(&self, p: &Parsed) -> anyhow::Result<u32> {
        let id = p.get("area").unwrap_or_default();
        let Some(area) = WorldArea::find(&self.data, id)? else {
            anyhow::bail!("no area {id} in WorldAreas");
        };
        let (topology, orientation) = (p.topology(), p.orientation());
        if topology.is_some() || orientation.is_some() {
            let entries = poe_world::area::topology_entries(&self.data, &area.id, 1).unwrap_or_default();
            if let Some(t) = topology
                && (t as usize > entries.len() || entries.is_empty())
            {
                anyhow::bail!("{} has {} topologies (1..={0})", area.id, entries.len());
            }
            if let (Some(o), Some(t)) = (orientation, topology) {
                let mask = entries[t as usize - 1].1;
                if mask != 0 && mask != 0xff && mask >> o & 1 == 0 {
                    anyhow::bail!("topology {t} of {} does not allow orientation {o} (mask {mask:#04x})", area.id);
                }
            }
        }
        let spec = InstanceSpec { area: area.id.clone(), seed: p.seed().or(area.fixed_seed()), topology, orientation };
        Ok(self.instances.create(spec, false))
    }

    /// Moves an online character to instance `id` from outside its session (another admin's
    /// command, the web API): the character as the login server hands it over.
    pub(crate) fn transfer_character(&self, account_id: u32, name: &str, id: u32, location: Option<(u32, i32, i32)>) -> anyhow::Result<()> {
        let Some(found) = self.store.characters(account_id)?.into_iter().find(|c| c.name.eq_ignore_ascii_case(name)) else {
            anyhow::bail!("{name} is not in the database");
        };
        let character = poe_cluster::TicketCharacter {
            id: found.id,
            name: found.name.clone(),
            league: found.league.clone(),
            class_id: found.class_id,
            level: found.level,
            intro_done: found.intro_done,
            location: None,
            arrive: None,
        };
        self.hand_off_at(account_id, character, id, location)?;
        // Ends the player's instance session; the client falls back to character selection on
        // its login connection, which delivers the pending InstanceDetails.
        self.live.kick(name);
        Ok(())
    }

    /// Hands this session's client to instance `id` (its login connection sends
    /// InstanceDetails; the client then leaves this instance).
    fn move_to(&self, ctx: &SessionCtx<'_>, id: u32) -> anyhow::Result<()> {
        self.hand_off(ctx.ticket.account_id, ctx.ticket.character.clone(), id)
    }

    /// Issues a ticket for instance `id` and gives it to the account's login connection.
    pub(crate) fn hand_off(&self, account_id: u32, character: poe_cluster::TicketCharacter, id: u32) -> anyhow::Result<()> {
        self.hand_off_at(account_id, character, id, None)
    }

    /// [`Self::hand_off`] to a place: `(seed, x, y)` is used when the instance has that seed.
    /// Otherwise a saved position belongs to the old layout and is dropped.
    pub(crate) fn hand_off_at(&self, account_id: u32, mut character: poe_cluster::TicketCharacter, id: u32, location: Option<(u32, i32, i32)>) -> anyhow::Result<()> {
        let Some(target) = self.instances.get(id) else {
            anyhow::bail!("no instance #{id}");
        };
        character.location = location;
        let ticket = self.tickets.issue_for(account_id, character, &target.spec.area, Some(id));
        if !self.sessions.transfer(account_id, ticket) {
            anyhow::bail!("no login connection for this account (cannot hand off)");
        }
        Ok(())
    }

    /// Appends an admin command to `[commands] audit_log`.
    pub(crate) fn audit(&self, account: &str, command: &str, result: &str) {
        use std::io::Write;
        let path = &self.config.commands.audit_log;
        if path.as_os_str().is_empty() {
            return;
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
        let line = format!("{ms}\t{account}\t{command}\t{result}\n");
        if let Err(e) = std::fs::OpenOptions::new().create(true).append(true).open(path).and_then(|mut f| f.write_all(line.as_bytes())) {
            warn!(path = %path.display(), "audit log: {e}");
        }
    }

    /// Whether an account may use admin commands: its role is admin and it is not disabled.
    pub(crate) fn is_admin(&self, account: &str) -> bool {
        match self.accounts.find(account) {
            Ok(found) => found.is_some_and(|a| a.role == poe_accounts::Role::Admin && !a.disabled),
            Err(e) => {
                warn!(account, "role lookup failed: {e}");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::frame_ms_for;

    #[test]
    fn time_scale_steps_are_whole_milliseconds_and_never_faster() {
        assert_eq!(frame_ms_for(1.0, 33), Some(33));
        assert_eq!(frame_ms_for(0.5, 33), Some(17));
        assert_eq!(frame_ms_for(0.03, 33), Some(1));
        for bad in [0.0, -1.0, 0.001, 1.01, 2.0] {
            assert_eq!(frame_ms_for(bad, 33), None, "{bad}");
        }
    }
}
