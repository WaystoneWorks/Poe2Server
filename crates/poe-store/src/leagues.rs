//! Leagues (migration 0009): families of a base league and its Hardcore / SSF / HC SSF
//! variants, managed in the web admin. The login server reads them for the league list and the
//! creation check; ending a family moves its characters into another family.
//!
//! Wire mapping (what the client gets for each row): docs/protocol/login.md, "LeagueEntry".

use std::collections::HashMap;

use rusqlite::{OptionalExtension, Row, Transaction, params};

use crate::{Result, SqliteStore};

/// Where a league is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeagueState {
    /// Listed and open for new characters (once started).
    Active,
    /// Not listed, no new characters; existing characters still play.
    Hidden,
    /// Over: its characters were moved to another family.
    Ended,
}

impl LeagueState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Hidden => "hidden",
            Self::Ended => "ended",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "active" => Some(Self::Active),
            "hidden" => Some(Self::Hidden),
            "ended" => Some(Self::Ended),
            _ => None,
        }
    }
}

/// The four members a family can have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Variant {
    Base,
    Hardcore,
    Ssf,
    HardcoreSsf,
}

impl Variant {
    pub const ALL: [Variant; 4] = [Self::Base, Self::Hardcore, Self::Ssf, Self::HardcoreSsf];

    pub fn of(hardcore: bool, ssf: bool) -> Self {
        match (hardcore, ssf) {
            (false, false) => Self::Base,
            (true, false) => Self::Hardcore,
            (false, true) => Self::Ssf,
            (true, true) => Self::HardcoreSsf,
        }
    }

    pub fn hardcore(self) -> bool {
        matches!(self, Self::Hardcore | Self::HardcoreSsf)
    }

    pub fn ssf(self) -> bool {
        matches!(self, Self::Ssf | Self::HardcoreSsf)
    }

    /// The variant's league id (and default display name) in a family named `name`, after the
    /// official pattern: "HC Dawn", "SSF Dawn", "HC SSF Dawn"; Standard's are "Hardcore",
    /// "Solo Self-Found" and "Hardcore SSF".
    pub fn league_id(self, name: &str) -> String {
        match (self, name) {
            (Self::Base, _) => name.to_owned(),
            (Self::Hardcore, "Standard") => "Hardcore".into(),
            (Self::Ssf, "Standard") => "Solo Self-Found".into(),
            (Self::HardcoreSsf, "Standard") => "Hardcore SSF".into(),
            (Self::Hardcore, _) => format!("HC {name}"),
            (Self::Ssf, _) => format!("SSF {name}"),
            (Self::HardcoreSsf, _) => format!("HC SSF {name}"),
        }
    }
}

/// One league (one row of the client's league list).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct League {
    /// Wire id, also stored on characters: "Dawn", "HC Dawn". Never changes.
    pub id: String,
    pub display_name: String,
    /// The base league's id (equal to `id` for the base).
    pub family: String,
    pub hardcore: bool,
    pub ssf: bool,
    /// `LeagueFlag.Id` of the banner.
    pub flag: Option<String>,
    /// `CoreLeagues.Id`s; stored only.
    pub mechanics: Vec<String>,
    /// A permanent family (Standard): no end, a target for ended leagues.
    pub permanent: bool,
    /// Unix seconds.
    pub starts_at: Option<i64>,
    pub ends_at: Option<i64>,
    pub state: LeagueState,
    /// The family characters move to when this one ends.
    pub migrate_to: Option<String>,
    /// The default family (its base is listed first and preselected); at most one.
    pub is_default: bool,
    pub sort: i64,
    pub created_at: i64,
}

impl League {
    pub fn is_base(&self) -> bool {
        self.id == self.family
    }

    pub fn variant(&self) -> Variant {
        Variant::of(self.hardcore, self.ssf)
    }

    /// Open for new characters at `now`.
    pub fn open_at(&self, now: i64) -> std::result::Result<(), LeagueClosed> {
        match self.state {
            LeagueState::Hidden => Err(LeagueClosed::Hidden),
            LeagueState::Ended => Err(LeagueClosed::Ended),
            LeagueState::Active if self.starts_at.is_some_and(|s| s > now) => {
                Err(LeagueClosed::NotStarted)
            }
            LeagueState::Active if self.ends_at.is_some_and(|e| e <= now) => {
                Err(LeagueClosed::Ended)
            }
            LeagueState::Active => Ok(()),
        }
    }
}

/// Why a league takes no new characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeagueClosed {
    Hidden,
    NotStarted,
    Ended,
}

/// A new family.
#[derive(Debug, Clone, Default)]
pub struct NewLeagueFamily {
    /// The base league's id and display name.
    pub name: String,
    pub hardcore: bool,
    pub ssf: bool,
    pub permanent: bool,
    /// `LeagueFlag.Id` per member; a missing one falls back to the base's.
    pub flags: HashMap<VariantKey, String>,
    pub mechanics: Vec<String>,
    pub starts_at: Option<i64>,
    pub ends_at: Option<i64>,
    pub migrate_to: Option<String>,
    pub is_default: bool,
}

/// `Variant` as a map key in [`NewLeagueFamily::flags`].
pub type VariantKey = Variant;

/// Changes to one league; `None` leaves a field as it is. The family-wide fields (`mechanics`,
/// `permanent`, dates, `migrate_to`, `is_default`) apply to every member of the family.
#[derive(Debug, Clone, Default)]
pub struct LeagueUpdate {
    pub display_name: Option<String>,
    pub flag: Option<Option<String>>,
    /// `Active` or `Hidden` (ending goes through [`LeagueStore::end_family`]).
    pub state: Option<LeagueState>,
    pub mechanics: Option<Vec<String>>,
    pub permanent: Option<bool>,
    pub starts_at: Option<Option<i64>>,
    pub ends_at: Option<Option<i64>>,
    pub migrate_to: Option<Option<String>>,
    pub is_default: Option<bool>,
}

/// A request the store refuses (the caller maps it to a 4xx).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LeagueRefusal {
    #[error("no league {0:?}")]
    NotFound(String),
    #[error("a league named {0:?} already exists")]
    Exists(String),
    #[error("invalid league name {0:?}: 1 to 40 letters, digits, spaces, - and '")]
    InvalidName(String),
    #[error("invalid display name: 1 to 60 characters")]
    InvalidDisplayName,
    #[error("the league has {0} characters")]
    HasCharacters(u32),
    #[error("{0}")]
    BadTarget(String),
    #[error("the league has ended")]
    Ended,
    #[error("the end date is before the start date")]
    BadDates,
    #[error("state can only be set to active or hidden")]
    BadState,
}

/// One character moved by [`LeagueStore::end_family`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeagueMove {
    pub character_id: u32,
    pub name: String,
    pub from: String,
    pub to: String,
}

pub type Refusable<T> = std::result::Result<T, LeagueRefusal>;

pub trait LeagueStore: Send + Sync {
    /// Every league: families by `sort`, members as base, HC, SSF, HC SSF.
    fn leagues(&self) -> Result<Vec<League>>;
    fn league(&self, id: &str) -> Result<Option<League>>;
    /// Creates the base and the variants `new` asks for.
    fn create_family(&self, new: &NewLeagueFamily, now: i64) -> Result<Refusable<Vec<League>>>;
    fn update_league(&self, id: &str, update: &LeagueUpdate) -> Result<Refusable<League>>;
    /// Ends `family` (a base id): every member's characters move to the member of
    /// `target` with the same flags (HC SSF falls back to the target's HC, everything else to
    /// the target's base when it has no such member), one
    /// `league_moves` line each; the family's state becomes ended.
    fn end_family(&self, family: &str, target: &str, now: i64) -> Result<Refusable<Vec<LeagueMove>>>;
    /// Deletes a variant, or a whole family when `id` is a base; refused while any of them has
    /// characters. Returns the deleted ids.
    fn delete_league(&self, id: &str) -> Result<Refusable<Vec<String>>>;
    /// Characters per league id.
    fn character_counts(&self) -> Result<HashMap<String, u32>>;
    /// Bases whose end date has passed and that have not ended yet.
    fn due_to_end(&self, now: i64) -> Result<Vec<League>>;
}

const COLUMNS: &str = "id, display_name, family, hardcore, ssf, flag, mechanics, permanent, \
    starts_at, ends_at, state, migrate_to, is_default, sort, created_at";

/// Family order, then base, HC, SSF, HC SSF (`ssf, hardcore` sorts them that way).
const ORDER: &str = "sort, family, ssf, hardcore";

fn league_row(r: &Row<'_>) -> rusqlite::Result<League> {
    let mechanics: String = r.get(6)?;
    let state: String = r.get(10)?;
    Ok(League {
        id: r.get(0)?,
        display_name: r.get(1)?,
        family: r.get(2)?,
        hardcore: r.get(3)?,
        ssf: r.get(4)?,
        flag: r.get(5)?,
        mechanics: serde_json::from_str(&mechanics).unwrap_or_default(),
        permanent: r.get(7)?,
        starts_at: r.get(8)?,
        ends_at: r.get(9)?,
        state: LeagueState::parse(&state).unwrap_or(LeagueState::Hidden),
        migrate_to: r.get(11)?,
        is_default: r.get(12)?,
        sort: r.get(13)?,
        created_at: r.get(14)?,
    })
}

/// League ids are what characters store and what the client sends back at creation.
pub fn valid_league_name(name: &str) -> bool {
    let len = name.chars().count();
    (1..=40).contains(&len)
        && name == name.trim()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '\''))
}

fn valid_display_name(name: &str) -> bool {
    let len = name.trim().chars().count();
    (1..=60).contains(&len) && !name.chars().any(char::is_control)
}

fn get(tx: &Transaction<'_>, id: &str) -> rusqlite::Result<Option<League>> {
    tx.query_row(
        &format!("SELECT {COLUMNS} FROM leagues WHERE id = ?1"),
        params![id],
        league_row,
    )
    .optional()
}

fn family_rows(tx: &Transaction<'_>, family: &str) -> rusqlite::Result<Vec<League>> {
    let mut st = tx.prepare(&format!(
        "SELECT {COLUMNS} FROM leagues WHERE family = ?1 ORDER BY {ORDER}"
    ))?;
    let rows = st.query_map(params![family], league_row)?;
    rows.collect()
}

fn characters_in(tx: &Transaction<'_>, ids: &[String]) -> rusqlite::Result<u32> {
    let mut n = 0;
    for id in ids {
        n += tx.query_row(
            "SELECT count(*) FROM characters WHERE league = ?1",
            params![id],
            |r| r.get::<_, u32>(0),
        )?;
    }
    Ok(n)
}

/// A family that characters may be moved into: an existing, permanent base that has not
/// ended and is not `family` itself.
fn check_target(tx: &Transaction<'_>, family: &str, target: &str) -> rusqlite::Result<Refusable<League>> {
    if target == family {
        return Ok(Err(LeagueRefusal::BadTarget(
            "a league cannot end into itself".into(),
        )));
    }
    Ok(match get(tx, target)? {
        Some(t) if !t.is_base() => Err(LeagueRefusal::BadTarget(format!(
            "{target:?} is a variant; pick its base league {:?}",
            t.family
        ))),
        Some(t) if t.state == LeagueState::Ended => Err(LeagueRefusal::BadTarget(format!(
            "{target:?} has ended"
        ))),
        Some(t) if !t.permanent => Err(LeagueRefusal::BadTarget(format!(
            "{target:?} is not a permanent league"
        ))),
        Some(t) => Ok(t),
        None => Err(LeagueRefusal::BadTarget(format!("no league {target:?}"))),
    })
}

impl LeagueStore for SqliteStore {
    fn leagues(&self) -> Result<Vec<League>> {
        self.with_connection(|conn| {
            let mut st = conn.prepare(&format!("SELECT {COLUMNS} FROM leagues ORDER BY {ORDER}"))?;
            let rows = st.query_map([], league_row)?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }

    fn league(&self, id: &str) -> Result<Option<League>> {
        self.with_connection(|conn| {
            Ok(conn
                .query_row(
                    &format!("SELECT {COLUMNS} FROM leagues WHERE id = ?1"),
                    params![id],
                    league_row,
                )
                .optional()?)
        })
    }

    fn create_family(&self, new: &NewLeagueFamily, now: i64) -> Result<Refusable<Vec<League>>> {
        if !valid_league_name(&new.name) {
            return Ok(Err(LeagueRefusal::InvalidName(new.name.clone())));
        }
        if let (Some(s), Some(e)) = (new.starts_at, new.ends_at)
            && e <= s
        {
            return Ok(Err(LeagueRefusal::BadDates));
        }
        self.with_connection(|conn| {
            let tx = conn.transaction()?;
            let members: Vec<Variant> = Variant::ALL
                .into_iter()
                .filter(|v| (!v.hardcore() || new.hardcore) && (!v.ssf() || new.ssf))
                .collect();
            for v in &members {
                let id = v.league_id(&new.name);
                if get(&tx, &id)?.is_some() {
                    return Ok(Err(LeagueRefusal::Exists(id)));
                }
            }
            if let Some(target) = &new.migrate_to
                && let Err(e) = check_target(&tx, &new.name, target)?
            {
                return Ok(Err(e));
            }
            if new.is_default {
                tx.execute("UPDATE leagues SET is_default = 0", [])?;
            }
            let sort: i64 = tx.query_row("SELECT coalesce(max(sort), 0) + 1 FROM leagues", [], |r| r.get(0))?;
            let mechanics = serde_json::to_string(&new.mechanics).unwrap_or_else(|_| "[]".into());
            let base_flag = new.flags.get(&Variant::Base).cloned();
            for v in &members {
                let id = v.league_id(&new.name);
                let flag = new.flags.get(v).cloned().or_else(|| base_flag.clone());
                tx.execute(
                    "INSERT INTO leagues (id, display_name, family, hardcore, ssf, flag, mechanics,
                         permanent, starts_at, ends_at, state, migrate_to, is_default, sort, created_at)
                     VALUES (?1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'active', ?10, ?11, ?12, ?13)",
                    params![
                        id,
                        new.name,
                        v.hardcore(),
                        v.ssf(),
                        flag,
                        mechanics,
                        new.permanent,
                        new.starts_at,
                        new.ends_at,
                        new.migrate_to,
                        new.is_default,
                        sort,
                        now
                    ],
                )?;
            }
            let rows = family_rows(&tx, &new.name)?;
            tx.commit()?;
            Ok(Ok(rows))
        })
    }

    fn update_league(&self, id: &str, u: &LeagueUpdate) -> Result<Refusable<League>> {
        self.with_connection(|conn| {
            let tx = conn.transaction()?;
            let Some(league) = get(&tx, id)? else {
                return Ok(Err(LeagueRefusal::NotFound(id.to_owned())));
            };
            if league.state == LeagueState::Ended {
                return Ok(Err(LeagueRefusal::Ended));
            }
            if let Some(name) = &u.display_name {
                if !valid_display_name(name) {
                    return Ok(Err(LeagueRefusal::InvalidDisplayName));
                }
                tx.execute("UPDATE leagues SET display_name = ?2 WHERE id = ?1", params![id, name.trim()])?;
            }
            if let Some(flag) = &u.flag {
                tx.execute("UPDATE leagues SET flag = ?2 WHERE id = ?1", params![id, flag])?;
            }
            if let Some(state) = u.state {
                if state == LeagueState::Ended {
                    return Ok(Err(LeagueRefusal::BadState));
                }
                tx.execute("UPDATE leagues SET state = ?2 WHERE id = ?1", params![id, state.as_str()])?;
            }
            let family = league.family.as_str();
            if let Some(m) = &u.mechanics {
                let json = serde_json::to_string(m).unwrap_or_else(|_| "[]".into());
                tx.execute("UPDATE leagues SET mechanics = ?2 WHERE family = ?1", params![family, json])?;
            }
            if let Some(p) = u.permanent {
                tx.execute("UPDATE leagues SET permanent = ?2 WHERE family = ?1", params![family, p])?;
            }
            let starts = u.starts_at.unwrap_or(league.starts_at);
            let ends = u.ends_at.unwrap_or(league.ends_at);
            if let (Some(s), Some(e)) = (starts, ends)
                && e <= s
            {
                return Ok(Err(LeagueRefusal::BadDates));
            }
            if u.starts_at.is_some() || u.ends_at.is_some() {
                tx.execute(
                    "UPDATE leagues SET starts_at = ?2, ends_at = ?3 WHERE family = ?1",
                    params![family, starts, ends],
                )?;
            }
            if let Some(target) = &u.migrate_to {
                if let Some(t) = target
                    && let Err(e) = check_target(&tx, family, t)?
                {
                    return Ok(Err(e));
                }
                tx.execute("UPDATE leagues SET migrate_to = ?2 WHERE family = ?1", params![family, target])?;
            }
            if let Some(d) = u.is_default {
                if d {
                    tx.execute("UPDATE leagues SET is_default = 0", [])?;
                }
                tx.execute("UPDATE leagues SET is_default = ?2 WHERE family = ?1", params![family, d])?;
            }
            let league = get(&tx, id)?.expect("updated row");
            tx.commit()?;
            Ok(Ok(league))
        })
    }

    fn end_family(&self, family: &str, target: &str, now: i64) -> Result<Refusable<Vec<LeagueMove>>> {
        self.with_connection(|conn| {
            let tx = conn.transaction()?;
            let members = family_rows(&tx, family)?;
            let Some(base) = members.iter().find(|l| l.is_base()) else {
                return Ok(Err(LeagueRefusal::NotFound(family.to_owned())));
            };
            if base.state == LeagueState::Ended {
                return Ok(Err(LeagueRefusal::Ended));
            }
            let target = match check_target(&tx, family, target)? {
                Ok(t) => t,
                Err(e) => return Ok(Err(e)),
            };
            let target_members = family_rows(&tx, &target.id)?;
            let mut moves = Vec::new();
            for m in &members {
                // The same variant; else, for HC SSF, the hardcore one; else the base.
                let find = |v: Variant| target_members.iter().find(|t| t.variant() == v);
                let to = find(m.variant())
                    .or_else(|| find(Variant::of(m.hardcore, false)))
                    .map_or(target.id.clone(), |t| t.id.clone());
                let mut st = tx.prepare("SELECT id, name FROM characters WHERE league = ?1 ORDER BY id")?;
                let chars: Vec<(u32, String)> = st
                    .query_map(params![m.id], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<_>>()?;
                drop(st);
                for (character_id, name) in chars {
                    tx.execute(
                        "INSERT INTO league_moves (character_id, name, from_league, to_league, at)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![character_id, name, m.id, to, now],
                    )?;
                    moves.push(LeagueMove {
                        character_id,
                        name,
                        from: m.id.clone(),
                        to: to.clone(),
                    });
                }
                tx.execute("UPDATE characters SET league = ?2 WHERE league = ?1", params![m.id, to])?;
            }
            tx.execute(
                "UPDATE leagues SET state = 'ended', is_default = 0, migrate_to = ?2 WHERE family = ?1",
                params![family, target.id],
            )?;
            tx.commit()?;
            Ok(Ok(moves))
        })
    }

    fn delete_league(&self, id: &str) -> Result<Refusable<Vec<String>>> {
        self.with_connection(|conn| {
            let tx = conn.transaction()?;
            let Some(league) = get(&tx, id)? else {
                return Ok(Err(LeagueRefusal::NotFound(id.to_owned())));
            };
            let ids: Vec<String> = if league.is_base() {
                family_rows(&tx, id)?.into_iter().map(|l| l.id).collect()
            } else {
                vec![league.id.clone()]
            };
            let n = characters_in(&tx, &ids)?;
            if n > 0 {
                return Ok(Err(LeagueRefusal::HasCharacters(n)));
            }
            for i in &ids {
                tx.execute("DELETE FROM leagues WHERE id = ?1", params![i])?;
            }
            // Families that would have ended into a deleted one fall back to "no target".
            if league.is_base() {
                tx.execute("UPDATE leagues SET migrate_to = NULL WHERE migrate_to = ?1 AND state != 'ended'", params![id])?;
            }
            tx.commit()?;
            Ok(Ok(ids))
        })
    }

    fn character_counts(&self) -> Result<HashMap<String, u32>> {
        self.with_connection(|conn| {
            let mut st = conn.prepare("SELECT league, count(*) FROM characters GROUP BY league")?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }

    fn due_to_end(&self, now: i64) -> Result<Vec<League>> {
        self.with_connection(|conn| {
            let mut st = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM leagues WHERE id = family AND state != 'ended'
                 AND ends_at IS NOT NULL AND ends_at <= ?1 ORDER BY {ORDER}"
            ))?;
            let rows = st.query_map(params![now], league_row)?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NewCharacter, Store};

    fn family(name: &str, hc: bool, ssf: bool, permanent: bool) -> NewLeagueFamily {
        NewLeagueFamily {
            name: name.into(),
            hardcore: hc,
            ssf,
            permanent,
            flags: HashMap::from([(Variant::Base, format!("{name}Flag"))]),
            ..Default::default()
        }
    }

    fn character(store: &SqliteStore, account: u32, name: &str, league: &str) -> u32 {
        store
            .create_character(&NewCharacter {
                account_id: account,
                name: name.into(),
                league: league.into(),
                class_id: 2,
                start_state: "DexFourDefault".into(),
                level: 1,
                area: "G1_1".into(),
            })
            .unwrap()
            .unwrap()
            .id
    }

    #[test]
    fn a_family_has_the_official_variants() {
        let store = SqliteStore::in_memory().unwrap();
        assert!(store.leagues().unwrap().is_empty());
        let rows = store.create_family(&family("Dawn", true, true, false), 10).unwrap().unwrap();
        let ids: Vec<_> = rows.iter().map(|l| l.id.as_str()).collect();
        assert_eq!(ids, ["Dawn", "HC Dawn", "SSF Dawn", "HC SSF Dawn"]);
        assert!(rows.iter().all(|l| l.family == "Dawn" && l.flag.as_deref() == Some("DawnFlag")));
        assert_eq!(rows[3].variant(), Variant::HardcoreSsf);
        let std = store.create_family(&family("Standard", true, false, true), 10).unwrap().unwrap();
        assert_eq!(std.iter().map(|l| l.id.as_str()).collect::<Vec<_>>(), ["Standard", "Hardcore"]);
        // Names are unique and checked.
        assert_eq!(
            store.create_family(&family("Dawn", false, false, false), 10).unwrap(),
            Err(LeagueRefusal::Exists("Dawn".into()))
        );
        assert!(matches!(
            store.create_family(&family("Bad/Name", false, false, false), 10).unwrap(),
            Err(LeagueRefusal::InvalidName(_))
        ));
        assert_eq!(store.leagues().unwrap().len(), 6);
    }

    #[test]
    fn default_state_and_dates() {
        let store = SqliteStore::in_memory().unwrap();
        let mut a = family("Standard", true, false, true);
        a.is_default = true;
        store.create_family(&a, 1).unwrap().unwrap();
        let mut b = family("Dawn", true, false, false);
        b.is_default = true;
        b.starts_at = Some(100);
        b.ends_at = Some(200);
        store.create_family(&b, 1).unwrap().unwrap();
        let defaults: Vec<_> = store.leagues().unwrap().into_iter().filter(|l| l.is_default).map(|l| l.id).collect();
        assert_eq!(defaults, ["Dawn", "HC Dawn"]);
        let hc = store.league("HC Dawn").unwrap().unwrap();
        assert_eq!(hc.open_at(50), Err(LeagueClosed::NotStarted));
        assert_eq!(hc.open_at(150), Ok(()));
        assert_eq!(hc.open_at(200), Err(LeagueClosed::Ended));
        let hidden = store
            .update_league("HC Dawn", &LeagueUpdate { state: Some(LeagueState::Hidden), ..Default::default() })
            .unwrap()
            .unwrap();
        assert_eq!(hidden.open_at(150), Err(LeagueClosed::Hidden));
        assert_eq!(store.league("Dawn").unwrap().unwrap().state, LeagueState::Active);
        // Family-wide fields go to every member; dates must be in order.
        let u = LeagueUpdate { ends_at: Some(Some(300)), mechanics: Some(vec!["Breach".into()]), ..Default::default() };
        store.update_league("Dawn", &u).unwrap().unwrap();
        let hc = store.league("HC Dawn").unwrap().unwrap();
        assert_eq!((hc.ends_at, hc.mechanics.clone()), (Some(300), vec!["Breach".to_string()]));
        let bad = LeagueUpdate { ends_at: Some(Some(50)), ..Default::default() };
        assert_eq!(store.update_league("Dawn", &bad).unwrap(), Err(LeagueRefusal::BadDates));
        assert_eq!(store.due_to_end(299).unwrap(), vec![]);
        assert_eq!(store.due_to_end(300).unwrap().iter().map(|l| l.id.as_str()).collect::<Vec<_>>(), ["Dawn"]);
    }

    #[test]
    fn ending_moves_each_variant_to_its_match() {
        let store = SqliteStore::in_memory().unwrap();
        let acc = store.create_account("k", "Exile").unwrap().id;
        // Standard has Hardcore but no SSF variants: SSF characters fall back to the base.
        store.create_family(&family("Standard", true, false, true), 1).unwrap().unwrap();
        store.create_family(&family("Dawn", true, true, false), 1).unwrap().unwrap();
        for (n, l) in [("Abase", "Dawn"), ("Bhc", "HC Dawn"), ("Cssf", "SSF Dawn"), ("Dhcssf", "HC SSF Dawn"), ("Estd", "Standard")] {
            character(&store, acc, n, l);
        }
        assert_eq!(
            store.end_family("Dawn", "Dawn", 5).unwrap(),
            Err(LeagueRefusal::BadTarget("a league cannot end into itself".into()))
        );
        assert!(matches!(store.end_family("Dawn", "Hardcore", 5).unwrap(), Err(LeagueRefusal::BadTarget(_))));
        let moves = store.end_family("Dawn", "Standard", 5).unwrap().unwrap();
        let got: Vec<_> = moves.iter().map(|m| (m.name.as_str(), m.to.as_str())).collect();
        assert_eq!(got, [("Abase", "Standard"), ("Bhc", "Hardcore"), ("Cssf", "Standard"), ("Dhcssf", "Hardcore")]);
        let leagues: Vec<_> = store.characters(acc).unwrap().into_iter().map(|c| c.league).collect();
        assert_eq!(leagues, ["Standard", "Hardcore", "Standard", "Hardcore", "Standard"]);
        assert!(store.leagues().unwrap().iter().filter(|l| l.family == "Dawn").all(|l| l.state == LeagueState::Ended));
        let audit: u32 = store.with_connection(|c| c.query_row("SELECT count(*) FROM league_moves", [], |r| r.get(0)).unwrap());
        assert_eq!(audit, 4);
        assert_eq!(store.end_family("Dawn", "Standard", 6).unwrap(), Err(LeagueRefusal::Ended));
        // An ended, empty family can be deleted; one with characters cannot.
        assert_eq!(store.delete_league("Standard").unwrap(), Err(LeagueRefusal::HasCharacters(5)));
        assert_eq!(store.delete_league("Dawn").unwrap().unwrap().len(), 4);
        assert_eq!(store.character_counts().unwrap().get("Hardcore"), Some(&2));
    }

    #[test]
    fn deleting_a_variant_keeps_the_family() {
        let store = SqliteStore::in_memory().unwrap();
        store.create_family(&family("Dawn", true, true, false), 1).unwrap().unwrap();
        assert_eq!(store.delete_league("SSF Dawn").unwrap().unwrap(), ["SSF Dawn"]);
        assert_eq!(store.leagues().unwrap().len(), 3);
        assert_eq!(store.delete_league("Nope").unwrap(), Err(LeagueRefusal::NotFound("Nope".into())));
    }
}
