//! [`AdminControl`] for the web API: the command registry and the live sessions, without a
//! game session of the caller's own.

use poe_cluster::{AdminControl, AdminOutcome, NewInstance, OnlinePlayer, TransferTarget};

use crate::{
    InstanceServer,
    commands::{self, Parsed},
    exec::Outcome,
};

fn outcome(o: Outcome) -> AdminOutcome {
    AdminOutcome {
        ok: o.ok,
        text: o.text,
    }
}

/// `instance new <area> [seed] [topology] [orientation]` for the registry's parser.
fn instance_line(new: &NewInstance) -> String {
    let mut line = format!("instance new {}", new.area);
    if new.seed.is_some() || new.topology.is_some() || new.orientation.is_some() {
        line += &format!(
            " {} {} {}",
            new.seed.map_or("random".into(), |s| s.to_string()),
            new.topology.map_or("natural".into(), |t| t.to_string()),
            new.orientation.map_or("natural".into(), |o| o.to_string())
        );
    }
    line
}

impl InstanceServer {
    /// Parses a line this file built (the caller checks the role).
    fn parse_internal(line: &str) -> Result<Parsed, String> {
        let (spec, rest) = commands::locate(line).map_err(|miss| miss.text(true))?;
        commands::parse_args(spec, rest)
    }

    fn web_label(actor: &str) -> String {
        format!("{actor} (web)")
    }

    fn create_instance_inner(&self, actor: &str, new: &NewInstance) -> Result<u32, AdminOutcome> {
        let line = instance_line(new);
        let parsed = Self::parse_internal(&line).map_err(AdminOutcome::err)?;
        if !self.is_admin(actor) {
            return Err(AdminOutcome::err("instance new is for admins only"));
        }
        let result = self.new_instance(&parsed);
        let id = result.as_ref().ok().copied();
        let result = result.map(|id| Outcome::ok(format!("created instance #{id}")));
        let out = self.finish(&Self::web_label(actor), &parsed, &line, result);
        id.ok_or(outcome(out))
    }
}

impl AdminControl for InstanceServer {
    fn online(&self) -> Vec<OnlinePlayer> {
        self.live
            .online()
            .into_iter()
            .map(|o| OnlinePlayer {
                muted: self.live.is_muted(&o.character),
                account_id: o.account_id,
                connected_at: o.connected_at,
                ip: o.ip.map(poe_accounts::canonical_ip),
                character: o.character,
                account: o.account,
                instance: o.instance,
                area: o.area,
            })
            .collect()
    }

    fn run_command(&self, actor: &str, line: &str) -> AdminOutcome {
        outcome(self.run_global(&Self::web_label(actor), actor, line))
    }

    fn create_instance(&self, actor: &str, new: &NewInstance) -> Result<u32, AdminOutcome> {
        self.create_instance_inner(actor, new)
    }

    fn transfer_player(
        &self,
        actor: &str,
        character: &str,
        target: TransferTarget,
    ) -> AdminOutcome {
        let spec = commands::find("instance transfer").expect("instance transfer is in the registry");
        let line = match &target {
            TransferTarget::Instance(id) => format!("transfer {character} -> instance #{id}"),
            TransferTarget::New(new) => format!(
                "transfer {character} -> {}",
                instance_line(new).replacen("instance new ", "", 1)
            ),
        };
        let result = (|| -> anyhow::Result<Outcome> {
            if !self.is_admin(actor) {
                return Ok(Outcome::err("transfer is for admins only"));
            }
            let Some((account_id, name)) = self.live.account_of(character) else {
                return Ok(Outcome::err(format!("{character} is not online")));
            };
            let id = match &target {
                TransferTarget::Instance(id) => *id,
                TransferTarget::New(new) => {
                    let parsed = Self::parse_internal(&instance_line(new)).map_err(anyhow::Error::msg)?;
                    self.new_instance(&parsed)?
                }
            };
            self.transfer_character(account_id, &name, id, None)?;
            Ok(Outcome::ok(format!("moving {name} to instance #{id}")))
        })();
        let parsed = Parsed {
            spec,
            args: Vec::new(),
            flags: Vec::new(),
        };
        outcome(self.finish(&Self::web_label(actor), &parsed, &line, result))
    }

    fn end_sessions(&self, account_id: u32) {
        self.sessions.supersede(account_id);
    }

    fn record(&self, actor: &str, action: &str, result: &str) {
        let label = Self::web_label(actor);
        tracing::info!(account = %label, command = %action, ok = true, result = %result, "command");
        self.audit(&label, action, result);
    }
}
