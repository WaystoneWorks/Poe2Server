//! Instance server: hosts areas. The login server hands clients over with a ticket;
//! the instance server validates it, sends the area and runs the simulation.
//!
//! Connection opening (docs/protocol/instance.md):
//! 1. client `Hello` (plaintext) carrying the hand-off ticket,
//! 2. server `EnableEncryption` (plaintext), then both directions switch to Salsa20 with the
//!    ticket's key material,
//! 3. server `AreaInfo` (area + seed): the client generates the terrain and loads the area.

use std::{sync::Arc, time::{Duration, Instant}};

use poe_cluster::{Ticket, TicketRegistry};
use poe_config::Config;
use poe_game::stats::StatRegistry;
use poe_store::Store;

pub mod actor;
mod admin;
mod appearance;
mod clock;
pub mod chat;
pub mod commands;
mod exec;
pub use exec::SERVER_NAME;
pub mod live;
pub mod names;
pub mod modlink;
mod movement;
mod player;
pub mod social;
pub mod side_stream;
mod spawn;
pub mod sync;
mod terrain_check;
mod travel;

#[cfg(test)]
mod actor_tests;
#[cfg(test)]
mod replay_tests;
#[cfg(test)]
mod mod_tests;
#[cfg(test)]
mod test_support;
use poe_data::{
    GameData,
    template::ObjectTemplate,
    views::{CharacterClass, InventorySlot, WorldArea},
};
use poe_net::{Connection, DecodeError, KeyMaterial, StreamCipher, hexdump::hexdump};
use poe_proto::{
    Message, Protocol, entity,
    instance::{
        AddInventory, AddObject, AreaInfo, CharacterState, ChatJoinRequest, ChatJoined,
        EnableEncryption, ExitReport, FrameReport, Hello, InitialState, Inventory, LeaveGame,
        LocalPlayers, PauseRequest, PingReply, PlayerEntity, PlayerState, SetPause,
        SimulationHeader, SimulationTick, StashTab, TerrainHashes, UpdateSkillTargetEntity,
    },
    remote::{PartyAccept, PartyDecline, PartyInvite, PartyInviteCancel, TradeCancel, TradeRequest},
};
use rand::RngCore;
use tokio::net::{TcpListener, TcpStream};
use tracing::{Instrument, debug, info, warn};

/// State shared by all instance connections.
pub struct InstanceServer {
    pub config: Arc<Config>,
    pub protocol: Arc<Protocol>,
    pub tickets: Arc<dyn TicketRegistry>,
    pub data: Arc<GameData>,
    /// Stat ids -> network keys (built once from Stats.dat).
    pub stats: Arc<StatRegistry>,
    pub store: Arc<dyn Store>,
    /// Leagues (the `league` command).
    pub leagues: Arc<dyn poe_store::LeagueStore>,
    /// Account roles (admin commands) and the `account` command.
    pub accounts: Arc<dyn poe_accounts::AccountStore>,
    /// Client position reports (drift checker and debug link, see [`sync`]).
    pub reports: Arc<sync::ClientReports>,
    /// Ends the account's login connection when the client leaves the game.
    pub sessions: Arc<dyn poe_cluster::LoginSessions>,
    /// Instances of areas: which one a session is in, its seed and layout choice.
    pub instances: Arc<dyn poe_cluster::InstanceDirectory>,
    /// Sessions in game (who, kick, chat delivery, mutes).
    pub live: Arc<live::Live>,
    /// The instance actors (one per instance with players, started by the first join).
    pub actors: Arc<actor::Actors>,
    /// The server's mods (events, commands, the login check of the client's mods).
    pub mods: Arc<poe_mod_host::ModHost>,
    /// Layouts the server chose (it always forces its layout), by area, seed and what the
    /// instance forces.
    pub layouts: LayoutCache,
    /// Where each character left last and its recent kicks (the terrain check's destinations).
    pub terrain_memory: terrain_check::Memory,
    /// When the server started (`uptime`).
    pub started: std::time::Instant,
    /// Game-data name lists of command arguments, read on first use.
    pub names: names::NameCache,
}

impl InstanceServer {
    pub async fn run(self: Arc<Self>) -> anyhow::Result<()> {
        let listener = TcpListener::bind(self.config.network.instance.bind).await?;
        info!(addr = %self.config.network.instance.bind, "instance server listening");
        if self.config.dev.position_sync {
            let reports = self.reports.clone();
            let addr = self.config.network.instance.bind;
            tokio::spawn(async move {
                if let Err(e) = reports.listen(addr).await {
                    warn!("client position reports: {e:#}");
                }
            });
        }
        // Idle instances close after `[instance] idle_minutes` (towns and hideouts stay).
        let idle = self.config.instance.idle_minutes;
        if idle > 0 {
            let instances = self.instances.clone();
            tokio::spawn(async move {
                let mut every = tokio::time::interval(Duration::from_secs(30));
                loop {
                    every.tick().await;
                    for id in instances.expire(Duration::from_secs(idle * 60)) {
                        info!(instance = id, "closed idle instance");
                    }
                }
            });
        }
        // Connections that have not shown a valid ticket yet are capped (a slot is freed once
        // the ticket is redeemed).
        let cfg = &self.config.network.instance;
        let limiter = poe_accounts::ConnectionLimiter::new(cfg.max_pre_auth_connections, cfg.max_pre_auth_per_ip);
        let mut last_refusal_log: Option<std::time::Instant> = None;
        loop {
            let (stream, peer) = listener.accept().await?;
            let Some(slot) = limiter.try_acquire(peer.ip()) else {
                if last_refusal_log.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
                    last_refusal_log = Some(std::time::Instant::now());
                    warn!(%peer, active = limiter.active(), "too many connections before the ticket: refused");
                }
                continue;
            };
            let span = tracing::info_span!("instance", %peer);
            let server = self.clone();
            tokio::spawn(
                async move {
                    // The side channel's TCP stream (the hook's mod report) shares the port: its
                    // first bytes are not a game opcode.
                    if side_stream::is_side_stream(&stream).await {
                        if let Err(e) = side_stream::serve(stream, &server.reports).await {
                            warn!("side stream: {e}");
                        }
                        return;
                    }
                    if let Err(e) = server.handle(stream, slot).await {
                        warn!("connection error: {e:#}");
                    }
                }
                .instrument(span),
            );
        }
    }

    async fn handle(&self, stream: TcpStream, mut slot: poe_accounts::ConnectionSlot) -> anyhow::Result<()> {
        let (client_port, client_ip) = {
            let peer = stream.peer_addr()?;
            (peer.port(), peer.ip())
        };
        let mut conn = Connection::new(stream)?;
        info!("client connected");
        let hello_op = self.protocol.instance.id(Hello::DIRECTION, Hello::NAME)?;
        // The first message must arrive complete within the read timeout and without piling up
        // bytes (`[network.instance] pre_auth_*`).
        let cfg = &self.config.network.instance;
        conn.set_max_buffer((cfg.pre_auth_max_buffer > 0).then_some(cfg.pre_auth_max_buffer));
        let read = conn.decode(|r| {
            let at = r.position();
            if r.u16()? != hello_op {
                return Err(DecodeError::Invalid {
                    offset: at,
                    what: "expected Hello",
                });
            }
            Hello::decode(r)
        });
        let hello = if cfg.pre_auth_read_timeout_secs > 0 {
            match tokio::time::timeout(Duration::from_secs(cfg.pre_auth_read_timeout_secs), read).await {
                Ok(r) => r?,
                Err(_) => {
                    warn!("no complete Hello in time: closing the connection");
                    return Ok(());
                }
            }
        } else {
            read.await?
        };
        let Some(hello) = hello else {
            return Ok(());
        };
        let Some(ticket) = self.tickets.redeem(hello.ticket) else {
            warn!(
                ticket = format_args!("{:#010x}", hello.ticket),
                "unknown or expired ticket"
            );
            return Ok(());
        };
        // A valid ticket: the connection no longer counts against the pre-ticket budget and
        // may carry the game's messages.
        slot.release();
        conn.set_max_buffer(None);
        info!(
            ticket = format_args!("{:#010x}", ticket.id),
            account = ticket.account_id,
            character = ticket.character.id,
            area = %ticket.area,
            "ticket redeemed"
        );

        // Side channel (position reports, debug link): the DLL derives the same
        // keys from the key material it gets in InstanceDetails (docs/protocol/side-channel.md).
        let side = SideSession {
            id: self.reports.register(&ticket.keys, client_port),
            reports: &self.reports,
        };
        info!(session = format_args!("{:016x}", side.id), "side-channel session");
        conn.send(
            &self
                .protocol
                .instance
                .encode(&EnableEncryption::default())?,
        )
        .await?;
        let keys = KeyMaterial(ticket.keys);
        conn.set_ciphers(
            Box::new(keys.client_to_server()),
            Box::new(keys.server_to_client()),
        );
        // HNC side channel (see PingReply): server-to-client keystream for idle bytes.
        let mut hnc = KeyMaterial(self.protocol.hnc.derive(&ticket.keys)).server_to_client();

        let loading = Instant::now();
        let (instance, seed, layout) = self.send_area(&mut conn, &ticket).await?;
        let instance = Some(instance);
        let hashes = match self.config.terrain.hash_timeout_secs {
            0 => self.await_hashes(&mut conn).await?,
            secs => {
                let left = Duration::from_secs(secs).saturating_sub(loading.elapsed());
                match tokio::time::timeout(left, self.await_hashes(&mut conn)).await {
                    Ok(h) => h?,
                    Err(_) => {
                        let m = self.mismatch(&ticket, instance, seed, layout, None, None, terrain_check::Reason::NoHashes { secs });
                        return self.kick_for_terrain(&mut conn, &ticket, side.id, m, false).await;
                    }
                }
            }
        };
        let Some(hashes) = hashes else {
            info!("client disconnected during terrain generation");
            return Ok(());
        };
        // hash_a is the tile hash, hash_b the doodad hash (as in the client log).
        info!(
            unk = hashes.unk,
            tile_hash = hashes.hash_a,
            doodad_hash = hashes.hash_b,
            "terrain generated by client"
        );
        // The server's own generation gives the grid, the spawn layout and the travel objects
        // and is the terrain check: tile and doodad hash must equal the client's, or the player
        // is kicked (`terrain_check`). An area the generator cannot make runs without a grid,
        // check or travel objects.
        let mut generated = match self.generated_terrain(&ticket.area, seed, instance).await {
            Ok(g) => Some(g),
            Err(e) => {
                warn!(area = %ticket.area, seed, "no generated terrain, the area runs without a server grid, terrain check or travel objects: {e}");
                None
            }
        };
        let mut kick = None;
        if let Some(g) = &generated {
            let server = g.hashes;
            let client = terrain_check::Hashes { tile: hashes.hash_a, doodad: hashes.hash_b };
            if let Some(reason) = terrain_check::compare(server, client) {
                let m = self.mismatch(&ticket, instance, seed, layout, Some(client), Some(server), reason);
                // The kick happens in the world: a client still on its loading screen ignores the
                // hand-off and reports an abnormal disconnect (game test, 2026-11-04).
                kick = Some(m);
            }
        }
        let objects = Arc::new(generated.as_mut().map_or(Vec::new(), |g| std::mem::take(&mut g.objects)));
        let generated = generated.map(|g| (g.grid, g.layout));
        // Where the character left this layout, if it did; else the area's start.
        let saved = ticket
            .character
            .location
            .filter(|&(s, _, _)| s == seed)
            .map(|(_, x, y)| (x, y));
        // Through a gate or waypoint: at the matching object.
        let arrival = ticket.character.arrive.and_then(|a| travel::arrival_cell(&objects, a));
        let spawn = match (spawn_override().or(arrival).or(saved), &generated) {
            (Some(p), _) => Some(p),
            (None, Some((grid, layout))) => spawn::from_layout(layout, grid, &self.config.spawn, &ticket.area),
            (None, None) => None,
        }
        .unwrap_or((0, 0));
        let grid = generated.map(|(grid, _)| grid);
        let mut cell = spawn;
        if saved.is_some() {
            // Same layout, so the saved cell is normally walkable; otherwise use the nearest
            // open spot.
            if let Some(grid) = &grid
                && !poe_game::movement::Walkable::walkable(grid, spawn.0, spawn.1)
                && let Some(nearest) = grid.nearest_walkable(spawn, 50)
            {
                cell = nearest;
            }
            info!(?cell, "restoring saved position");
        }
        let Some(instance_id) = instance else {
            anyhow::bail!("no instance for the session");
        };
        let name = ticket.character.name.clone();
        let account = self
            .store
            .account_by_id(ticket.account_id)
            .ok()
            .flatten()
            .map(|a| a.name)
            .unwrap_or_else(|| format!("account {}", ticket.account_id));
        // The client's mods against the server's requirements (the hook reports them over the
        // side channel while the terrain was generating); the answer is acted on once the
        // client is in the world and can show a message.
        let mod_verdict = self.client_mod_verdict(side.id).await;
        // Join the instance's actor. The saved or start cell is the spawn; the session's
        // entity id comes from the instance's counter.
        let session = self.actors.next_session();
        let setup = self.new_player(&ticket)?;
        let Some(class) = CharacterClass::by_integer_id(&self.data, ticket.character.class_id)?
        else {
            anyhow::bail!("unknown class id {}", ticket.character.class_id);
        };
        // New characters wash up on their first visit to an intro area.
        let washed_up =
            !ticket.character.intro_done && self.config.spawn.intro_areas.contains(&ticket.area);
        let unlocked = self.store.waypoints(ticket.character.id)?;
        let quest_flags = travel::quest_flags(&self.data, &unlocked.iter().cloned().collect())?;
        let appearance = self.appearance(&ticket.character, &class, cell, &setup.skills, washed_up, quest_flags)?;
        let (frames_tx, mut frames) = tokio::sync::mpsc::unbounded_channel();
        let grid: Option<Arc<dyn poe_game::grid::GridSource>> = grid.map(|g| Arc::new(g) as _);
        let request = actor::JoinRequest {
            session,
            name: name.clone(),
            account: account.clone(),
            admin: self.is_admin(&account),
            spawn: cell,
            movement: setup.movement.clone(),
            grid,
            appearance,
            want_debug: self.config.dev.drift_check || self.config.dev.debug_link,
            out: frames_tx,
            objects,
        };
        let (handle, joined) = self
            .actors
            .join(&*self.instances, instance_id, |r| self.actor_init(&ticket, instance_id, r), request)
            .await?;
        // From here on the player leaves the instance however the session ends (also on `?`).
        let mut player = player::Player::new(session, handle, joined.entity, setup.skills, joined.own.clone());
        if washed_up {
            self.store.set_intro_done(ticket.character.id)?;
        }
        let touched = self.store.checkpoints(ticket.character.id, &ticket.area, seed)?;
        let travel = travel::Travel::new(joined.objects.clone(), unlocked, touched);
        let table = &self.protocol.instance;
        let mut entry = table.encode(&poe_proto::instance::WaypointMask { unk: 0, entity: 0, mask: travel::waypoint_mask(&self.data, &travel.waypoints())? })?;
        entry.extend(table.encode(&travel.warp_list(&ticket.area))?);
        self.enter_world(&mut conn, &ticket, &class, washed_up, &joined, &player, &entry).await?;
        if let Some(m) = kick {
            // Neither remembered as a previous place nor saved as the character's location.
            let result = self.kick_for_terrain(&mut conn, &ticket, side.id, m, true).await;
            player.leave().await;
            return result;
        }
        if let poe_mod_host::Verdict::Refused(problems) = mod_verdict {
            self.refuse_client_mods(&mut conn, &name, &problems).await?;
            player.leave().await;
            return Ok(());
        }

        let ctx = exec::SessionCtx {
            travel: &travel,
            ticket: &ticket,
            account: account.clone(),
            instance,
            seed,
            tile_hash: hashes.hash_a,
            port: client_port,
            side: side.id,
        };
        // Another login of the account (or disabling it) ends this session.
        let superseded = self.sessions.subscribe_superseded(ticket.account_id);
        let registration = self.live.register_from(ticket.account_id, &account, &name, instance, &ticket.area, session, Some(client_ip));
        self.live.set_character(&name, &ticket.character.league, ticket.character.level);
        let social_in = self.live.social.register(self.social_person(&ticket, &account, instance_id, player.entity_id, session));
        self.send_registry(&ctx);
        let closed = self.instances.closed(instance_id);
        let result = self
            .run_session(
                &mut conn,
                &mut hnc,
                &ctx,
                &mut player,
                &mut frames,
                Stops { closed, superseded, kicked: registration.kicked },
                registration.chat,
                social_in,
            )
            .await;
        self.live.unregister(&name, session);
        self.live.social.unregister(&name, session);
        // Leaving tells the other players in the instance (removal sequence) and returns the
        // cell the character stood on.
        let (x, y) = player.leave().await;
        // Save where the character is, however the session ended.
        self.terrain_memory.remember(
            ticket.character.id,
            terrain_check::Previous { instance: instance_id, area: ticket.area.clone(), seed, cell: (x, y) },
        );
        let location = poe_store::Location { seed, x, y };
        match self
            .store
            .save_location(ticket.character.id, &ticket.area, Some(location))
        {
            Ok(()) => info!(area = %ticket.area, seed, cell = ?(x, y), "saved character location"),
            Err(e) => warn!("saving character location: {e}"),
        }
        result
    }

    /// Compares the client's reported mods with the server's required set
    /// ([`poe_mod_host::check_client`]). Waits for the report only when the answer depends on
    /// it: the check is on and the server requires mods or forbids extra ones.
    async fn client_mod_verdict(&self, side: u64) -> poe_mod_host::Verdict {
        use poe_mod_host::Verdict;
        let mods = &self.mods;
        if !mods.check_enabled() || (mods.required().is_empty() && mods.allow_extra_client_mods()) {
            return Verdict::Allowed;
        }
        let wait = Duration::from_secs(mods.report_wait_secs());
        let report = self.reports.wait_mod_report(side, wait).await;
        if report.is_none() {
            warn!(waited = ?wait, "the client sent no mod report (an old hook, or none)");
        }
        poe_mod_host::check_client(mods.required(), mods.allow_extra_client_mods(), report.as_ref())
    }

    /// Tells the client why it is refused (server chat lines), gives it a moment to show them
    /// and ends the session.
    async fn refuse_client_mods(&self, conn: &mut Connection, name: &str, problems: &[String]) -> anyhow::Result<()> {
        warn!(player = name, ?problems, "login refused: the client's mods do not match this server's");
        let mut lines = vec!["You cannot play on this server: your client mods do not match it.".to_owned()];
        lines.extend(problems.iter().cloned());
        let needed: Vec<String> = self.mods.required().iter().map(|m| format!("{} {}", m.id, m.version)).collect();
        if !needed.is_empty() {
            lines.push(format!("This server needs: {}. Install them with the launcher (server page), then log in again.", needed.join(", ")));
        }
        for text in lines {
            let line = live::ChatOut { sender: exec::SERVER_NAME.into(), text };
            conn.send(&self.protocol.instance.encode(&chat_line(&line))?).await?;
        }
        tokio::time::sleep(Duration::from_secs(8)).await;
        Ok(())
    }

    /// What the actor of `instance` is created with (the first join's character supplies the
    /// default movement configuration and the grid).
    fn actor_init(&self, ticket: &Ticket, instance: u32, first: &actor::JoinRequest) -> actor::ActorInit {
        let stats = self.stats.clone();
        actor::ActorInit {
            instance,
            area: ticket.area.clone(),
            tick_ms: self.config.instance.tick_ms.max(1) as u32,
            first_entity_id: self.config.instance.first_entity_id,
            movement: first.movement.clone(),
            model: Arc::new(poe_game::locomotion::ClientModel),
            grid: first.grid.clone(),
            codec: {
                let public = stats.clone();
                movement::Codec::new(self.protocol.clone(), Arc::new(move |id: &str| stats.key(id)))
                    .with_stat_visibility(Arc::new(move |id: &str| public.is_party(id)))
            },
            directory: self.instances.clone(),
            idle: Duration::from_secs(self.config.instance.idle_minutes * 60),
            player_spacing: self.config.spawn.player_spacing,
            mods: self.mods.clone(),
            sink: Arc::new(modlink::LiveSink(self.live.clone())),
        }
    }

    /// The in-game loop: client messages and the instance actor's frames until the session
    /// ends. The session is a pipe: it forwards client input to the actor and writes the frames
    /// the actor sends (`head`, then the events and the `SimulationTick` marker).
    #[allow(clippy::too_many_arguments)]
    async fn run_session(
        &self,
        conn: &mut Connection,
        hnc: &mut poe_net::Salsa20Stream,
        ctx: &exec::SessionCtx<'_>,
        player: &mut player::Player,
        frames: &mut tokio::sync::mpsc::UnboundedReceiver<actor::Frame>,
        mut stops: Stops,
        mut chat_in: tokio::sync::mpsc::UnboundedReceiver<live::ChatOut>,
        mut social_in: tokio::sync::mpsc::UnboundedReceiver<social::SocialOut>,
    ) -> anyhow::Result<()> {
        let client_port = ctx.port;
        // Drift checker (dev): server position against the client's reports, logged only.
        let mut drift = self.config.dev.drift_check.then(|| {
            sync::DriftCheck::new(Duration::from_millis(self.config.dev.drift_interval_ms.max(1)))
        });
        let drift_started = std::time::Instant::now();
        let mut report_seen = false;
        let mut ticks: u64 = 0;
        // Not decoded yet: log the client's decrypted messages (heartbeat 0x1c is skipped).
        loop {
            tokio::select! {
                stop = stops.wait() => {
                    match stop {
                        Stop::InstanceClosed => info!("instance closed: disconnecting the player"),
                        Stop::Superseded => info!("account logged in again or was disabled: disconnecting the player"),
                        Stop::Kicked => info!("kicked: disconnecting the player"),
                    }
                    return Ok(());
                }
                Some(line) = chat_in.recv() => {
                    conn.send(&self.protocol.instance.encode(&chat_line(&line))?).await?;
                }
                // Party and trade messages of other players' requests (docs/protocol/entities.md).
                Some(out) = social_in.recv() => {
                    conn.send(&out.encode(&self.protocol.instance)?).await?;
                }
                n = conn.read_more() => {
                    if n? == 0 {
                        info!("client disconnected");
                        return Ok(());
                    }
                    let data = conn.buffer().to_vec();
                    conn.consume(data.len());
                    // Client messages have no length prefix: decode the known ones in turn;
                    // anything else ends the batch (logged).
                    let mut rest = &data[..];
                    while rest.len() >= 2 {
                        let opcode = u16::from_be_bytes([rest[0], rest[1]]);
                        let mut r = poe_net::Reader::new(&rest[2..]);
                        match self.handle_client_message(conn, hnc, player, ctx, opcode, &mut r).await? {
                            Handled::Continue => rest = &rest[2 + r.position()..],
                            Handled::Unknown => {
                                info!(len = rest.len(), "received\n{}", hexdump(rest));
                                break;
                            }
                            Handled::Exit => return Ok(()),
                        }
                    }
                }
                frame = frames.recv() => {
                    // The actor ended (instance closed or idle): so does the session.
                    let Some(frame) = frame else {
                        info!("instance ended: disconnecting the player");
                        return Ok(());
                    };
                    let started = std::time::Instant::now();
                    ticks += 1;
                    player.apply(frame.own.clone());
                    self.live.set_cell(&ctx.ticket.character.name, player.cell());
                    let mut world = self.touch_objects(ctx, player.cell())?;
                    if ticks.is_multiple_of(30) {
                        world.extend(self.refresh_unlocks(ctx, player.entity_id)?);
                    }
                    let mut handed_off = false;
                    for &(entity, flags) in &frame.interactions {
                        let (bytes, leave) = self.interact(ctx, player, entity, flags)?;
                        world.extend(bytes);
                        handed_off |= leave;
                    }
                    if let Some(check) = drift.as_mut() {
                        let report = self.reports.fresh(ctx.side);
                        report_seen |= report.is_some();
                        if !report_seen && drift_started.elapsed() > Duration::from_secs(5) {
                            report_seen = true;
                            warn!(
                                client_port,
                                "drift check is on but the client sends no position reports \
                                 (poe-hook position_sync, [dev] position_sync)"
                            );
                        }
                        if let Some(l) = check.sample(player.position(), || player.state().to_string(), report.as_ref()) {
                            info!(
                                server = format_args!("{:.2},{:.2}", l.server.0, l.server.1),
                                client = ?l.client.map(|c| format!("{:.2},{:.2}", c.0, c.1)),
                                cells = ?l.cells.map(|d| format!("{d:.2}")),
                                max_cells = ?l.max_cells.map(|d| format!("{d:.2}")),
                                samples = l.samples,
                                state = %l.state,
                                "drift"
                            );
                        }
                    }
                    let mut leave = handed_off;
                    let mut out = world;
                    if self.config.dev.debug_link {
                        // Overlay commands: the same registry as chat commands.
                        for command in self.reports.take_commands(ctx.side) {
                            if command == "registry" {
                                self.send_registry(ctx);
                                continue;
                            }
                            if let Some(request) = command.strip_prefix("complete ") {
                                self.send_completion(ctx, request);
                                continue;
                            }
                            let cmd = self.run_command(ctx, player, &command);
                            out.extend(cmd.raw);
                            leave |= cmd.leave;
                            let status = if cmd.ok { "ok" } else { "error" };
                            self.reports.send_debug(
                                ctx.side,
                                &format!("ack\t{command}\t{status}: {}\n", cmd.text.replace('\t', " ").replace('\n', " | ")),
                            );
                        }
                        if ticks.is_multiple_of(30) {
                            self.send_instances(ctx);
                            self.send_state(ctx, player);
                        }
                        if ticks.is_multiple_of(2) {
                            self.reports.send_debug(
                                ctx.side,
                                &format!(
                                    "tick\t{ticks}\n{}",
                                    player.debug_state(self.reports.fresh(ctx.side).as_ref())
                                ),
                            );
                        }
                    }
                    out.extend_from_slice(&frame.head);
                    out.extend_from_slice(&frame.events);
                    conn.send(&out).await?;
                    if leave {
                        info!("handing the client to another instance: ending this session");
                        return Ok(());
                    }
                    let took = started.elapsed();
                    if took.as_millis() as u64 > self.config.instance.tick_ms / 2 {
                        warn!(?took, "slow session frame");
                    }
                }
            }
        }
    }

    /// What instance `instance` forces itself (`transfer ... topology orientation`) and whether
    /// the server completes it with its own choice; resolve it with
    /// [`resolve_layout`] off the async threads.
    fn layout_request(&self, instance: Option<u32>) -> LayoutRequest {
        let spec = instance.and_then(|id| self.instances.get(id)).map(|i| i.spec);
        LayoutRequest {
            data: self.data.clone(),
            cache: self.layouts.clone(),
            given: poe_world::area::Forced { topology: spec.as_ref().and_then(|s| s.topology), orientation: spec.as_ref().and_then(|s| s.orientation) },
        }
    }

    /// The collision grid and spawn layout the server generates for the instance's layout
    /// with the hashes of that layout: `poe-world` makes the `PWG1`
    /// bytes and the layout TSV, `poe-game` and `spawn` read them, so the crates stay independent
    /// (docs/architecture/instance-world.md in the hub). `Err` when the generator fails (no
    /// check is made then and the area has no server grid).
    async fn generated_terrain(&self, area: &str, seed: u32, instance: Option<u32>) -> Result<GeneratedTerrain, String> {
        let data = self.data.clone();
        let area = area.to_string();
        let request = self.layout_request(instance);
        let skip_objects = self.config.travel.skip_objects.clone();
        let started = std::time::Instant::now();
        let result = tokio::task::spawn_blocking(move || {
            let forced = resolve_layout(&request, &area, seed);
            let mut rng = poe_world::rng::TerrainRng::new(seed);
            // One generation reads the same tile and room files many times: cached, G1_15 takes
            // 3.6 s instead of 36 s (the client leaves after about 30 s without the area).
            let files = poe_world::generate::CachedFiles::new(data.files());
            let g = poe_world::area::generate_with_files(&data, &files, &area, &mut rng, forced).map_err(|e| e.to_string())?;
            let hashes = terrain_check::Hashes { tile: g.map.tile_hash(), doodad: g.doodad_hash() };
            // The client's live grid is its own generation (client side) plus the hull of every static
            // entity the server sends (`0x141d0ce70`). This server sends none of the `server_only`
            // static objects yet, so the grid it must agree with is the client side one; a
            // `Side::Server` grid would block cells (healing wells, benches) the client walks through.
            let mut objects = poe_world::objects::travel_objects(&data, &area, &g).map_err(|e| e.to_string())?;
            objects.retain(|o| !skip_objects.iter().any(|skip| o.kind.contains(skip.as_str()) || o.ao.contains(skip.as_str())));
            // The objects this server sends with a hull change the client's grid: its wells.
            let wells: std::collections::BTreeSet<(i32, i32)> = objects.iter().filter(|o| o.class == poe_world::objects::Class::Well).map(|o| (o.x, o.y)).collect();
            let sent: Vec<poe_world::doodads::Doodad> = g
                .doodads
                .iter()
                .chain(&g.tile_objects)
                .filter(|d| d.solid && d.exists(poe_world::doodads::Side::Server) && !d.exists(poe_world::doodads::Side::Client) && wells.contains(&(d.x, d.y)))
                .map(|d| poe_world::doodads::Doodad { side: 0, ..d.clone() })
                .collect();
            let grid = poe_world::collision::build(&g.map, &files, g.doodads.iter().chain(&g.tile_objects).chain(&sent), poe_world::doodads::Side::Client).map_err(|e| e.to_string())?;
            let grid = spawn::Grid::parse(&grid.pwg1()).ok_or_else(|| "generated grid is not PWG1".to_string())?;
            let layout = spawn::Layout::parse(&g.map.layout_tsv()).ok_or_else(|| "generated layout does not parse".to_string())?;
            let here = poe_data::views::WorldArea::find(&data, &area).ok().flatten();
            let town = here.as_ref().is_some_and(|a| a.is_town);
            let objects = travel::prepare(objects, &grid, town, here.map_or(0, |a| a.hash16));
            Ok::<_, String>(GeneratedTerrain { grid, layout, hashes, objects })
        })
        .await
        .map_err(|e| format!("generation task: {e}"))?;
        if let Ok(g) = &result {
            info!(seed, took = ?started.elapsed(), cells = ?(g.grid.cells_x, g.grid.cells_y), "generated the collision grid");
        }
        result
    }

    /// Server-side terrain (`terrain` command): generates the layout in
    /// the background and logs whether its tile hash matches the client's.
    pub(crate) fn generate_terrain(&self, area: &str, seed: u32, client_hash: u32, instance: Option<u32>) {
        // At most `GENERATE_LIMIT` generations at once (on by default, so a burst of new
        // instances must not take every core): the others are skipped, it is only a check.
        static RUNNING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        const GENERATE_LIMIT: usize = 2;
        if RUNNING.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= GENERATE_LIMIT {
            RUNNING.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            info!(%area, seed, "server terrain skipped: {GENERATE_LIMIT} generations already running");
            return;
        }
        let data = self.data.clone();
        let area = area.to_string();
        // The instance's forced layout, as sent in AreaInfo.
        let request = self.layout_request(instance);
        tokio::task::spawn_blocking(move || {
            // Frees the slot also when the generator panics.
            struct Slot;
            impl Drop for Slot {
                fn drop(&mut self) {
                    RUNNING.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                }
            }
            let _slot = Slot;
            let forced = resolve_layout(&request, &area, seed);
            let started = std::time::Instant::now();
            let mut rng = poe_world::rng::TerrainRng::new(seed);
            let files = poe_world::generate::CachedFiles::new(data.files());
            match poe_world::area::generate_with_files(&data, &files, &area, &mut rng, forced) {
                Ok(g) => {
                    let ours = g.map.tile_hash();
                    if ours == client_hash {
                        info!(%area, seed, tile_hash = ours, took = ?started.elapsed(), "server terrain matches the client");
                    } else {
                        warn!(%area, seed, ours, client = client_hash, graph = %g.graph, "server terrain differs from the client");
                    }
                }
                Err(e) => warn!(%area, seed, "server terrain: {e}"),
            }
        });
    }

    /// Waits for the client's TerrainHashes (sent after it generated the area).
    async fn await_hashes(&self, conn: &mut Connection) -> anyhow::Result<Option<TerrainHashes>> {
        let hashes_op = self
            .protocol
            .instance
            .id(TerrainHashes::DIRECTION, TerrainHashes::NAME)?;
        Ok(conn
            .decode(|r| {
                let at = r.position();
                if r.u16()? != hashes_op {
                    return Err(DecodeError::Invalid {
                        offset: at,
                        what: "expected TerrainHashes",
                    });
                }
                TerrainHashes::decode(r)
            })
            .await?)
    }

    /// The `area <id> <seed>` dev command's AreaInfo message.
    fn area_info_command(&self, args: &str, ticket: &Ticket) -> anyhow::Result<Vec<u8>> {
        let mut words = args.split_whitespace();
        let (Some(id), Some(seed)) = (words.next(), words.next()) else {
            anyhow::bail!("usage: area <WorldAreas.Id> <seed>");
        };
        let Some(area) = WorldArea::find(&self.data, id)? else {
            anyhow::bail!("no area {id}");
        };
        let msg = AreaInfo {
            instance_id: rand::random(),
            area_hash: area.hash16,
            league: ticket.character.league.clone(),
            seed: seed.parse()?,
            ..Default::default()
        };
        info!(area = %area.id, seed = msg.seed, "dev: sending area");
        Ok(self.protocol.instance.encode(&msg)?)
    }

    /// Sends the area; returns the terrain seed (a saved location's seed when the character
    /// left this area, so it comes back to the same layout).
    /// A stat's network key (`Stats` row + 1) by id.
    fn stat_key(&self, id: &str) -> anyhow::Result<u16> {
        self.stats.key(id).map(|k| k as u16).ok_or_else(|| anyhow::anyhow!("no stat {id}"))
    }

    /// The instance a ticket enters: its own, else the area's newest, else a new one.
    fn ticket_instance(&self, ticket: &Ticket) -> anyhow::Result<u32> {
        if let Some(id) = ticket.instance
            && self.instances.get(id).is_some()
        {
            return Ok(id);
        }
        if let Some(id) = self.instances.find(&ticket.area) {
            return Ok(id);
        }
        let Some(area) = WorldArea::find(&self.data, &ticket.area)? else {
            anyhow::bail!("ticket area {} not in game data", ticket.area);
        };
        Ok(self.instances.create(
            poe_cluster::InstanceSpec { area: area.id.clone(), seed: area.fixed_seed(), ..Default::default() },
            area.is_town || area.is_hideout,
        ))
    }

    /// AreaInfo for the session's instance; returns the instance and its seed.
    async fn send_area(&self, conn: &mut Connection, ticket: &Ticket) -> anyhow::Result<(u32, u32, Option<poe_world::area::Forced>)> {
        let Some(area) = WorldArea::find(&self.data, &ticket.area)? else {
            anyhow::bail!("ticket area {} not in game data", ticket.area);
        };
        let instance_id = self.ticket_instance(ticket)?;
        let seed = {
            let mut rng = rand::rng();
            // RE experiment: `POE_SEED` pins the layout seed (to reproduce a layout).
            let pinned = std::env::var("POE_SEED").ok().and_then(|v| v.parse().ok());
            let saved = ticket.character.location.map(|(seed, _, _)| seed);
            let spec_seed = self.instances.get(instance_id).and_then(|i| i.spec.seed);
            let seed = pinned
                .or(spec_seed)
                .or(area.fixed_seed())
                .or(saved)
                .unwrap_or_else(|| rng.next_u32());
            // The first player decides an instance's seed; later ones share it.
            self.instances.settle_seed(instance_id, seed).unwrap_or(seed)
        };
        // The layout (`transfer ... topology orientation`, else the server's own choice): the generator reads `map_force_topology` (1-based) and
        // `map_force_orientation` (orientation + 1).
        let layout = {
            let (request, area_id) = (self.layout_request(Some(instance_id)), area.id.clone());
            tokio::task::spawn_blocking(move || resolve_layout(&request, &area_id, seed)).await?
        };
        let mut forced = Vec::new();
        if let Some(t) = layout.and_then(|l| l.topology) {
            forced.push((self.stat_key("map_force_topology")?, t as i32));
        }
        if let Some(o) = layout.and_then(|l| l.orientation) {
            forced.push((self.stat_key("map_force_orientation")?, o as i32 + 1));
        }
        let msg = AreaInfo {
            instance_id,
            area_hash: area.hash16,
            // Official servers send the character's league here.
            league: ticket.character.league.clone(),
            seed,
            stats: (!forced.is_empty()).then(|| poe_proto::instance::AreaStats { first: forced.clone(), second: Vec::new(), more: Vec::new() }),
            ..Default::default()
        };
        if !forced.is_empty() {
            info!(?forced, "forcing the layout (AreaInfo stats)");
        }
        info!(area = %area.id, seed = msg.seed, instance = msg.instance_id, "sending area");
        conn.send(&self.protocol.instance.encode(&msg)?).await?;
        Ok((instance_id, seed, layout))
    }
}

/// A ChatMessage for a chat line (layout as in official captures).
fn chat_line(line: &live::ChatOut) -> poe_proto::instance::ChatMessage {
    poe_proto::instance::ChatMessage { unk_10: 1, sender: line.sender.clone(), text: line.text.clone(), unk_a2: 5, ..Default::default() }
}

impl InstanceServer {
    /// The command registry for the overlay (debug link): `cmds` datagrams of `cmdspec` lines
    /// (`commands::wire`), only what this account may use, then who it is.
    fn send_registry(&self, ctx: &exec::SessionCtx<'_>) {
        if !self.config.dev.debug_link {
            return;
        }
        let admin = self.is_admin(&ctx.account);
        let mut chunk = String::from("cmds\tbegin\n");
        for spec in commands::all().filter(|c| c.visible_to(admin)) {
            let line = commands::wire(spec);
            if chunk.len() + line.len() > 3000 {
                self.reports.send_debug(ctx.side, &chunk);
                chunk = String::from("cmds\tmore\n");
            }
            chunk.push_str(&line.replace('\n', " "));
            chunk.push('\n');
        }
        chunk.push_str(&format!("you\t{}\t{}\t{}\n", ctx.account, admin as u8, self.config.commands.prefix));
        self.reports.send_debug(ctx.side, &chunk);
    }

    /// Answers `complete <kind> <typed>`: `completion\t<kind>\t<typed>` and a `name\t<id>\t<display>`
    /// line per match. Player, instance and league names are for admins only.
    fn send_completion(&self, ctx: &exec::SessionCtx<'_>, request: &str) {
        let (kind, typed) = request.split_once(' ').unwrap_or((request, ""));
        let Some(parsed) = commands::Kind::listed(kind) else {
            return;
        };
        let private = matches!(parsed, commands::Kind::Player | commands::Kind::Instance | commands::Kind::League);
        let list = if private && !self.is_admin(&ctx.account) { None } else { commands::Names::names(self, parsed) };
        let mut out = format!("completion\t{kind}\t{typed}\n");
        for n in list.as_deref().map_or(Vec::new(), |l| commands::complete(l, typed, names::COMPLETIONS)) {
            let line = format!("name\t{}\t{}\n", n.id, n.display);
            if out.len() + line.len() > 3000 {
                break;
            }
            out.push_str(&line);
        }
        self.reports.send_debug(ctx.side, &out);
    }

    /// Live values for the overlay's Debug tabs: `state\t<key>\t<value>` lines, and for admins
    /// the online players as `online` lines (`character, account, area, instance, league, level,
    /// muted`).
    fn send_state(&self, ctx: &exec::SessionCtx<'_>, player: &player::Player) {
        let character = &ctx.ticket.character;
        let tick = self.config.instance.tick_ms.max(1) as f64;
        let (x, y) = player.cell();
        let mut out = String::from("state\tbegin\n");
        for (key, value) in [
            ("character", character.name.clone()),
            ("level", character.level.to_string()),
            ("class", character.class_id.to_string()),
            ("league", character.league.clone()),
            ("area", ctx.ticket.area.clone()),
            ("instance", ctx.instance.map_or("-".into(), |i| i.to_string())),
            ("seed", ctx.seed.to_string()),
            ("cell", format!("{x} {y}")),
            ("time_scale", format!("{:.2}", f64::from(player.frame_ms()) / tick)),
            ("muted", (self.live.is_muted(&character.name) as u8).to_string()),
        ] {
            out.push_str(&format!("state\t{key}\t{value}\n"));
        }
        if self.is_admin(&ctx.account) {
            for o in self.live.online() {
                let line = format!(
                    "online\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                    o.character,
                    o.account,
                    o.area,
                    o.instance.map_or("-".into(), |i| i.to_string()),
                    o.league,
                    o.level,
                    self.live.is_muted(&o.character) as u8
                );
                // Enough for a private server; the rest stays in `who`.
                if out.len() + line.len() > 3000 {
                    break;
                }
                out.push_str(&line);
            }
        }
        self.reports.send_debug(ctx.side, &out);
    }

    /// The instance list for the overlay's Instances tab: `instances` datagrams of `inst`
    /// lines (`id, area, seed, topology, orientation, persistent, empty seconds, current,
    /// players`).
    fn send_instances(&self, ctx: &exec::SessionCtx<'_>) {
        let list = self.instances.list();
        let mut chunk = String::from("instances\tbegin\n");
        for i in &list {
            let line = format!(
                "inst\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                i.id,
                i.spec.area,
                i.spec.seed.map_or(String::new(), |s| s.to_string()),
                i.spec.topology.map_or(String::new(), |t| t.to_string()),
                i.spec.orientation.map_or(String::new(), |o| o.to_string()),
                i.persistent as u8,
                i.empty_since.map_or(String::new(), |t| t.elapsed().as_secs().to_string()),
                (Some(i.id) == ctx.instance) as u8,
                i.players.join(",")
            );
            if chunk.len() + line.len() > 3000 {
                self.reports.send_debug(ctx.side, &chunk);
                chunk = String::from("instances\tmore\n");
            }
            chunk.push_str(&line);
        }
        chunk.push_str(&format!("idle\t{}\n", self.config.instance.idle_minutes));
        self.reports.send_debug(ctx.side, &chunk);
    }
}

/// The ways an in-game session is ended from outside.
pub(crate) struct Stops {
    /// The instance is closed in the directory (admin `close`, idle expiry).
    pub closed: Option<tokio::sync::watch::Receiver<bool>>,
    /// The account logged in again or was disabled ([`poe_cluster::LoginSessions::supersede`]).
    pub superseded: tokio::sync::watch::Receiver<u64>,
    /// An admin kicked the session ([`live::Live::kick`]).
    pub kicked: tokio::sync::watch::Receiver<bool>,
}

/// Which of the [`Stops`] fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stop {
    InstanceClosed,
    Superseded,
    Kicked,
}

impl Stops {
    pub async fn wait(&mut self) -> Stop {
        tokio::select! {
            () = instance_closed(&mut self.closed) => Stop::InstanceClosed,
            () = account_superseded(&mut self.superseded) => Stop::Superseded,
            () = was_kicked(&mut self.kicked) => Stop::Kicked,
        }
    }
}

/// Resolves when the session is kicked.
async fn was_kicked(rx: &mut tokio::sync::watch::Receiver<bool>) {
    if rx.wait_for(|k| *k).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Resolves when the account's sessions are superseded; never when the registry is gone.
async fn account_superseded(rx: &mut tokio::sync::watch::Receiver<u64>) {
    if rx.changed().await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Resolves when the session's instance is closed; never without an instance.
async fn instance_closed(rx: &mut Option<tokio::sync::watch::Receiver<bool>>) {
    if let Some(rx) = rx {
        // A closed instance's sender is dropped: an error also means closed.
        let _ = rx.wait_for(|c| *c).await;
        return;
    }
    std::future::pending().await
}

/// RE experiment: `POE_NPC=<metadata path>` adds that object as entity 2 next to the player
/// with a zero blob (Positioned first). Objects with a StateMachine need their shared states
/// (see docs/protocol/entities.md), so this suits simple types such as
/// `Metadata/MiscellaneousObjects/Waypoint`.
fn npc_experiment(
    table: &poe_proto::OpcodeTable,
    path: &str,
    (x, y): (i32, i32),
) -> anyhow::Result<Vec<u8>> {
    let mut blob = vec![entity::ENTITY_IN_WAKE_ZONE, 0];
    blob.extend((x + 15).to_le_bytes());
    blob.extend(y.to_le_bytes());
    blob.extend([0u8; 1024]);
    info!(%path, "adding experiment object");
    Ok(table.encode(&AddObject {
        header: SimulationHeader {
            entity_id: 2,
            ..Default::default()
        },
        type_key: poe_data::hash::object_type_key(path),
        blob,
    })?)
}

/// RE experiment: `POE_SPAWN=x,y` forces the grid position (the client clamps it to the map).
fn spawn_override() -> Option<(i32, i32)> {
    let v = std::env::var("POE_SPAWN").ok()?;
    let (x, y) = v.split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

/// A registered side-channel session; dropped (however the connection ends) it removes the
/// session from the report registry.
struct SideSession<'a> {
    id: u64,
    reports: &'a sync::ClientReports,
}

impl Drop for SideSession<'_> {
    fn drop(&mut self) {
        self.reports.forget(self.id);
    }
}

/// Outcome of handling one client message.
pub(crate) enum Handled {
    Continue,
    /// Not a message the server decodes yet: the rest of the batch is skipped.
    Unknown,
    /// The session ends.
    Exit,
}

impl InstanceServer {
    /// One client message after the player entered the world (`r` is after the opcode).
    #[allow(clippy::too_many_arguments)]
    async fn handle_client_message(
        &self,
        conn: &mut Connection,
        hnc: &mut poe_net::Salsa20Stream,
        player: &mut player::Player,
        ctx: &exec::SessionCtx<'_>,
        opcode: u16,
        r: &mut poe_net::Reader<'_>,
    ) -> anyhow::Result<Handled> {
        let account_id = ctx.ticket.account_id;
        let table = &self.protocol.instance;
        let Some(name) = table.name(poe_proto::Direction::Client, opcode) else {
            return Ok(Handled::Unknown);
        };
        // Movement input (WASD, skills, sprint, aim): queued in the simulation, answered by the
        // next tick (docs/protocol/movement.md).
        if let Some(input) = movement::client_input(name, r)? {
            player.input(input);
            return Ok(Handled::Continue);
        }
        if let Some(handled) = self.travel_message(conn, ctx, player, name, r).await? {
            return Ok(handled);
        }
        match name {
            // Frame report: answer with a ping reply carrying idle HNC bytes.
            FrameReport::NAME => {
                FrameReport::decode(r)?;
                let mut bytes = [0u8; 4];
                hnc.apply(&mut bytes);
                let reply = PingReply {
                    hnc: poe_proto::instance::hnc_bits(bytes),
                    ..Default::default()
                };
                conn.send(&table.encode(&reply)?).await?;
            }
            UpdateSkillTargetEntity::NAME => {
                let req = UpdateSkillTargetEntity::decode(r)?;
                debug!(?req, "skill target entity (no entities yet)");
            }
            // Escape menu: in a solo game confirm with SetPause, which pauses the client's
            // world; the simulation pauses with it. With other players in the instance the
            // actor refuses (the world keeps running) and the client is told "not paused".
            PauseRequest::NAME => {
                let req = PauseRequest::decode(r)?;
                let applied = player.set_paused(req.paused).await;
                info!(paused = req.paused, applied, "pause request");
                let paused = req.paused && applied;
                let reply = SetPause {
                    paused,
                    flag_869: paused,
                    ..Default::default()
                };
                conn.send(&table.encode(&reply)?).await?;
            }
            // A typed chat line: a server command, or a line for its channel.
            poe_proto::instance::ChatSend::NAME => {
                let req = poe_proto::instance::ChatSend::decode(r)?;
                let name = &ctx.ticket.character.name;
                match chat::route(&req.text, &self.config.commands.prefix) {
                    chat::Route::Command(line) => {
                        let out = self.run_command(ctx, player, &line);
                        if !out.raw.is_empty() {
                            conn.send(&out.raw).await?;
                        }
                        let leave = out.leave;
                        // A mod's own command answers with the chat lines it submits.
                        if !(out.ok && out.text.is_empty()) {
                            let text = if out.ok { out.text } else { format!("Error: {}", out.text) };
                            // Long output (help pages) is one chat line per line.
                            let mut lines = Vec::new();
                            for text in text.lines() {
                                let line = live::ChatOut { sender: exec::SERVER_NAME.into(), text: text.to_owned() };
                                lines.extend(table.encode(&chat_line(&line))?);
                            }
                            conn.send(&lines).await?;
                        }
                        if leave {
                            info!("handing the client to another instance: ending this session");
                            return Ok(Handled::Exit);
                        }
                    }
                    route => {
                        info!(from = %name, text = %req.text, "chat");
                        if let Some(err) = chat::deliver(&self.live, name, ctx.instance, route) {
                            let line = live::ChatOut { sender: exec::SERVER_NAME.into(), text: err };
                            conn.send(&table.encode(&chat_line(&line))?).await?;
                        }
                    }
                }
            }
            // Party and trade requests (WP C3): invite and cancel of a party, request and cancel
            // of a trade. Accepting and declining are not decoded (gate G-PARTY).
            PartyInvite::NAME => {
                let req = PartyInvite::decode(r)?;
                info!(from = %ctx.ticket.character.name, to = %req.name, "party invite");
                let result = self.live.social.invite(&ctx.ticket.character.name, &req.name);
                self.social_reply(conn, result).await?;
            }
            PartyInviteCancel::NAME => {
                let req = PartyInviteCancel::decode(r)?;
                info!(from = %ctx.ticket.character.name, to = %req.name, "party invite cancel");
                let result = self.live.social.cancel_invite(&ctx.ticket.character.name, &req.name);
                self.social_reply(conn, result).await?;
            }
            TradeRequest::NAME => {
                let req = TradeRequest::decode(r)?;
                info!(from = %ctx.ticket.character.name, target = req.target, "trade request");
                let result = self.live.social.trade_request(&ctx.ticket.character.name, req.target);
                self.social_reply(conn, result).await?;
            }
            TradeCancel::NAME => {
                let req = TradeCancel::decode(r)?;
                info!(from = %ctx.ticket.character.name, trade = req.trade, "trade cancel");
                let result = self.live.social.trade_cancel(&ctx.ticket.character.name, req.trade);
                self.social_reply(conn, result).await?;
            }
            // Read from the client's popup callbacks, never captured: not answered.
            PartyAccept::NAME | PartyDecline::NAME => {
                let (accept, party) = if name == PartyAccept::NAME {
                    (true, PartyAccept::decode(r)?.party)
                } else {
                    (false, PartyDecline::decode(r)?.party)
                };
                warn!(
                    from = %ctx.ticket.character.name,
                    party,
                    accept,
                    "party answer not implemented: needs capture G-PARTY (docs/protocol/entities.md#party-and-trade-flows-wp-c3)"
                );
            }
            // Chat channels: the requested number (or `[chat] default_channel` for "any").
            ChatJoinRequest::NAME => {
                let req = ChatJoinRequest::decode(r)?;
                let channel = if req.channel == 0xffff {
                    self.config.chat.default_channel
                } else {
                    req.channel
                };
                info!(kind = req.kind, channel, "chat join");
                self.live.set_channel(&ctx.ticket.character.name, req.kind != 0, channel);
                let reply = ChatJoined {
                    unk_10: req.unk_0,
                    channel,
                    trade: req.kind != 0,
                    unk_1c: req.unk_4,
                };
                conn.send(&table.encode(&reply)?).await?;
            }
            // Exit to character selection: hand the client a resume ticket (0x174). It then
            // reconnects to the login server and presents it (login 0x08) instead of
            // authenticating; the login server answers like a normal login.
            "ExitRequest" => {
                let resume = self.tickets.issue_resume(account_id);
                info!("client exits to character selection");
                let reply = poe_proto::instance::ExitSession {
                    unk_10: 0,
                    unk_20: resume.account_id,
                    unk_18: resume.token,
                };
                conn.send(&table.encode(&reply)?).await?;
                return Ok(Handled::Exit);
            }
            ExitReport::NAME => {
                let report = ExitReport::decode(r)?;
                debug!(len = report.data.len(), "exit report");
            }
            // "Exit to log in screen" or quitting: the client closes the connection next. Its
            // login connection must close too, or it returns to character selection on it.
            LeaveGame::NAME => {
                LeaveGame::decode(r)?;
                info!("client leaves the game");
                self.sessions.logout(account_id);
            }
            _ => match table.client_body_size(opcode) {
                // Known size, not decoded yet: skip it.
                Some(size) => {
                    let body = r.bytes(size)?;
                    debug!(opcode = format_args!("{opcode:#06x}"), ?body, "skipped");
                }
                None => return Ok(Handled::Unknown),
            },
        }
        Ok(Handled::Continue)
    }

    /// What the party and trade code needs to know about a session's character.
    fn social_person(&self, ticket: &Ticket, account: &str, instance: u32, entity: u32, session: u64) -> social::Person {
        let area = WorldArea::find(&self.data, &ticket.area).ok().flatten();
        social::Person {
            character: ticket.character.name.clone(),
            account: account.to_owned(),
            level: ticket.character.level.min(255) as u8,
            class: ticket.character.class_id.min(255) as u8,
            area: area.as_ref().map_or(0, |a| a.hash16),
            is_town: area.as_ref().is_some_and(|a| a.is_town),
            league: ticket.character.league.clone(),
            instance: Some(instance),
            entity,
            local_index: LOCAL_PLAYER,
            session,
        }
    }

    /// A request's refusal goes to the player as a server chat line.
    async fn social_reply(&self, conn: &mut Connection, result: Result<(), String>) -> anyhow::Result<()> {
        if let Err(text) = result {
            let line = live::ChatOut { sender: exec::SERVER_NAME.into(), text };
            conn.send(&self.protocol.instance.encode(&chat_line(&line))?).await?;
        }
        Ok(())
    }

    /// The character's server-side state: walking speed (class template
    /// `Pathfinding.base_speed`, cells per second), the innate skills (`[player]
    /// innate_skills`) and the movement configuration they and the class give.
    fn new_player(&self, ticket: &Ticket) -> anyhow::Result<PlayerSetup> {
        let Some(class) = CharacterClass::by_integer_id(&self.data, ticket.character.class_id)?
        else {
            anyhow::bail!("unknown class id {}", ticket.character.class_id);
        };
        let template = ObjectTemplate::load(&self.data, &class.metadata_id)?;
        let speed = template
            .component("Pathfinding")
            .iter()
            .find(|(k, _)| k == "base_speed")
            .and_then(|(_, v)| v.parse::<f32>().ok())
            .unwrap_or(37.0);
        let mut skills = Vec::new();
        for (i, id) in self.config.player.innate_skills.iter().enumerate() {
            match poe_data::views::GrantedSkill::find(&self.data, id, 1)? {
                Some(granted) => {
                    // The stat set's constant stats, and its implicit (flag) stats as 1. The
                    // client reads computed stats (e.g. `dodge_roll_travel_distance`), which
                    // the server derives from the base ones; until there is a stat calculator,
                    // each `..._base_...` stat is also sent under its name without
                    // `base_` when that stat exists.
                    let mut named: Vec<(String, i32)> = granted
                        .constant_stats
                        .iter()
                        .map(|(k, v)| (k.clone(), *v as i32))
                        .chain(granted.implicit_stats.iter().map(|k| (k.clone(), 1)))
                        .collect();
                    let derived: Vec<(String, i32)> = named
                        .iter()
                        .filter(|(k, _)| k.contains("base_"))
                        .map(|(k, v)| (k.replacen("base_", "", 1), *v))
                        .collect();
                    named.extend(derived);
                    let mut stats = Vec::new();
                    for (stat, value) in &named {
                        if let Some(key) = poe_data::views::stat_key(&self.data, stat)? {
                            stats.push((key, *value));
                        } else if !stat.contains("base_") {
                            debug!(%stat, "skill stat not in Stats.dat");
                        }
                    }
                    skills.push(player::OwnedSkill {
                        // Small ids, distinct from basic actions (0x2000xxxx) and item skills.
                        id: i as u32 + 1,
                        granted,
                        id_name: id.clone(),
                        stats,
                    })
                }
                None => warn!(skill = %id, "innate skill not in game data"),
            }
        }
        let move_skill = poe_data::views::action_type_key(&self.data, "Move")?
            .map(|k| player::BASIC_ACTION_SKILL | k as u32);
        info!(speed, skills = skills.len(), "player state");
        // Sprint speed (dodge roll held), from the class template.
        let sprint_bonus = template
            .component("Stats")
            .iter()
            .find(|(k, _)| k == "sprint_movement_speed_+%")
            .and_then(|(_, v)| v.parse::<f32>().ok());
        let roll_action = poe_data::views::action_type_key(&self.data, "DodgeRoll")?;
        // The client grows the entity's pathing size to the larger of `object_size` and
        // `secondary_object_size` (4) from the end of a roll until the sprint ends.
        let size = |key: &str| {
            template
                .component("Positioned")
                .iter()
                .find(|(k, _)| k == key)
                .and_then(|(_, v)| v.parse::<i32>().ok())
        };
        let sprint_clearance = size("object_size")
            .unwrap_or(0)
            .max(size("secondary_object_size").unwrap_or(0));
        let mut movement = player::sim_config(
            &self.config.movement,
            &skills,
            move_skill,
            roll_action,
            speed,
            sprint_bonus,
        );
        movement.sprint_clearance_level = sprint_clearance;
        if let Some(key) = poe_data::views::action_type_key(&self.data, "Interaction")? {
            movement.interact_skill = player::BASIC_ACTION_SKILL | u32::from(key);
        }
        movement.interact_range = self.config.travel.interact_range;
        let template_stat = |key: &str| {
            template.component("Stats").iter().find(|(k, _)| k == key).and_then(|(_, v)| v.parse::<f32>().ok())
        };
        if let Some(rate) = template_stat("dodge_roll_sprint_turn_rate") {
            movement.sprint_turn_rate = rate;
        }
        movement.speed_stats = poe_game::stats::speed_stats(&poe_game::stats::character_defaults(
            &template,
            &class,
            ticket.character.level,
        ));
        // Required: the client's roll and sprint end move by these curves.
        use anyhow::Context as _;
        let curves = poe_game::motion_data::MotionCurves::load(self.data.files())
            .context("reading the movement curves from the game data")?
            .context("no movement curves in the game data (basedodgeroll, basesprint onerig.amd)")?;
        movement.curves = Some(Arc::new(curves));
        movement.offset_scale =
            poe_game::motion_data::load_offset_scale(self.data.files(), &class.metadata_id)
                .with_context(|| format!("no offset_scale in {}.ao or its parents", class.metadata_id))?;
        Ok(PlayerSetup { skills, movement })
    }

    /// After the client generated the area and the actor accepted the session: local player,
    /// the player entity (`0x24`), inventories, the entities (`joined.add_objects`: the other
    /// players first, then this one), then InitialState (see docs/protocol/instance.md and
    /// docs/protocol/entities.md).
    async fn enter_world(
        &self,
        conn: &mut Connection,
        ticket: &Ticket,
        class: &CharacterClass,
        washed_up: bool,
        joined: &actor::Joined,
        player: &player::Player,
        travel_state: &[u8],
    ) -> anyhow::Result<()> {
        let table = &self.protocol.instance;
        let character = &ticket.character;
        let spawn = player.cell();
        // Local player 1 (as on official servers: the client copies the index into its own
        // messages, e.g. `0062 01 ..`).
        let players = LocalPlayers {
            unk: 0,
            players: vec![(LOCAL_PLAYER, character.name.clone())],
        };
        conn.send(&table.encode(&players)?).await?;
        // Official order: the player's entity id, then its inventories.
        conn.send(&table.encode(&PlayerEntity {
            unk: LOCAL_PLAYER,
            local_index: LOCAL_PLAYER,
            entity_id: player.entity_id,
        })?)
        .await?;

        // The inventories an official server creates (modules/protocol/opcodes/<version>/inventories.tsv),
        // else one empty inventory per Inventories.dat slot.
        let mut out = Vec::new();
        let official = load_inventories(&self.config.game.protocol_dir.join("inventories.tsv"));
        if !official.is_empty() {
            for (id, inventory) in official {
                out.extend(table.encode(&AddInventory {
                    unk_10: LOCAL_PLAYER,
                    inventory_id: id,
                    inventory,
                    ..Default::default()
                })?);
            }
        } else {
            let cfg = &self.config.inventories;
            for slot in InventorySlot::all(&self.data)? {
                if cfg.skip.contains(&slot.id) {
                    continue;
                }
                let [width, height] = cfg.size(&slot.id);
                out.extend(table.encode(&AddInventory {
                    unk_10: LOCAL_PLAYER,
                    inventory_id: slot.index as u32 + 1,
                    inventory: Inventory {
                        id_key: slot.id_key as u8,
                        slot: slot.index,
                        width,
                        height,
                        flags: 0,
                    },
                    ..Default::default()
                })?);
            }
        }
        conn.send(&out).await?;

        // The entities: the other players (remote form), then this player's own AddObject, all
        // built by the actor with the entity ids of its counter (layout: poe_proto::entity,
        // poe_proto::player_blob).
        let mut out = joined.add_objects.clone();
        if let Ok(path) = std::env::var("POE_NPC") {
            out.extend(npc_experiment(table, &path, spawn)?);
        }
        out.extend(table.encode(&SimulationTick)?);
        // Official: the waypoint mask at every entry, before InitialState.
        out.extend_from_slice(travel_state);
        conn.send(&out).await?;

        // InitialState must follow 0x24 and the player entity (its handler uses the entity).
        // `frame_ms` is the length of each simulation frame.
        // Flag values not understood yet are the official ones (docs/re/captures.md).
        // `flags` bits 0/1 start the client's world paused: the client unpauses itself with a
        // PauseRequest only in zones; in a town it never sends one (official town capture:
        // flags 4, no PauseRequest), so a town started with bits 0/1 stays frozen. Hideouts
        // are unverified: 4 is safe there, because the world never starts paused.
        let town = WorldArea::find(&self.data, &ticket.area)?.is_some_and(|a| a.is_town || a.is_hideout);
        let initial = InitialState {
            frame_ms: self.config.instance.tick_ms as u32,
            flags: if town { 0x04 } else { 0x07 },
            characters: vec![CharacterState {
                stash_tabs: (NEW_ACCOUNT_STASH_TABS.iter().enumerate())
                    .map(|(i, name)| StashTab::plain(i as u16, name))
                    .collect(),
                flags: [
                    // Bits 3 and 5 (+0x3ece, +0x3ed0); bit 6: account mode 0 (else 3).
                    0x68,
                    // Full game access: no "Get Full Access" button in the escape menu.
                    poe_proto::instance::CHARACTER_FULL_ACCESS,
                    0,
                ],
                ..Default::default()
            }],
            players: vec![PlayerState {
                // The client matches it against the entity id of `0x24` (the capture's
                // first zone has 0x5b in both, the town 12013).
                player_id: player.entity_id,
                // Character 0, bit 6 (entry +7).
                character: 0x40,
                flags: 0x07,
                ..Default::default()
            }],
            ..Default::default()
        };
        conn.send(&table.encode(&initial)?).await?;
        info!(
            class = %class.metadata_id,
            ?spawn,
            washed_up,
            entity = player.entity_id,
            players = joined.players,
            "player entered the world"
        );
        Ok(())
    }
}

/// The stash tabs every account has (InitialState), as an official new account gets them until
/// stashes are stored.
const NEW_ACCOUNT_STASH_TABS: [&str; 4] = ["1", "2", "3", "4"];

/// What the session knows about its character before it joins the instance.
struct PlayerSetup {
    skills: Vec<player::OwnedSkill>,
    /// Includes the walking speed.
    movement: poe_game::movement::MovementConfig,
}

/// The local player index the server gives the character (official servers use 1).
const LOCAL_PLAYER: u8 = 1;

/// `inventories.tsv`: `id, id_key, slot, width, height, flags` per line (`#` comments, a
/// header line). Empty when the file is missing.
fn load_inventories(path: &std::path::Path) -> Vec<(u32, Inventory)> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let num = |s: &str| -> Option<u32> {
        match s.strip_prefix("0x") {
            Some(h) => u32::from_str_radix(h, 16).ok(),
            None => s.parse().ok(),
        }
    };
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.starts_with("id\t"))
        .filter_map(|l| {
            let f: Vec<u32> = l.split('\t').map(num).collect::<Option<_>>()?;
            let [id, id_key, slot, width, height, flags] = f[..] else {
                return None;
            };
            Some((
                id,
                Inventory {
                    id_key: id_key as u8,
                    slot: slot as u8,
                    width: width as u8,
                    height: height as u8,
                    flags: flags as u8,
                },
            ))
        })
        .collect()
}

/// Cache of [`resolve_layout`] results by (area, seed, what the instance forces).
pub type LayoutCache = Arc<std::sync::Mutex<std::collections::HashMap<(String, u32, poe_world::area::Forced), Option<poe_world::area::Forced>>>>;

/// What the server generates for an area ([`InstanceServer::generated_terrain`]).
struct GeneratedTerrain {
    grid: spawn::Grid,
    layout: spawn::Layout,
    hashes: terrain_check::Hashes,
    objects: Vec<travel::PreparedObject>,
}

/// See [`InstanceServer::layout_request`].
struct LayoutRequest {
    data: Arc<GameData>,
    cache: LayoutCache,
    given: poe_world::area::Forced,
}

/// The layout an instance loads with, as AreaInfo forces it (WP7.13): what the instance forces,
/// completed with the generator's natural choice (`poe_world::area::choose_layout`, equal to the
/// client's own). `None` only when the generator cannot choose (the client chooses then). The
/// choice is the same with and without forcing it, so this only makes client and server agree by
/// construction.
fn resolve_layout(r: &LayoutRequest, area: &str, seed: u32) -> Option<poe_world::area::Forced> {
    let given = (r.given.topology.is_some() || r.given.orientation.is_some()).then_some(r.given);
    let key = (area.to_string(), seed, r.given);
    if let Some(f) = r.cache.lock().unwrap().get(&key) {
        return *f;
    }
    let chosen = match poe_world::area::choose_layout(&r.data, r.data.files(), area, seed, r.given) {
        Ok(f) => Some(f),
        Err(e) => {
            warn!(%area, seed, "no server layout choice, the client chooses: {e}");
            given
        }
    };
    r.cache.lock().unwrap().insert(key, chosen);
    chosen
}
