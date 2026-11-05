//! Leagues: `GET /leagues`, `/admin/leagues` (list, create, update, end, delete) and the game
//! data the league editor offers (`/admin/data/league-flags`, `/admin/data/league-mechanics`).
//!
//! Rules (names, variants, ending, deletion) are the store's ([`poe_store::LeagueStore`]) and
//! [`crate::leagues`]; this module is the HTTP face. Times are unix seconds. Endpoints that
//! need the game data (a flag, mechanics, art URLs) answer `503` when the server has none.

use std::collections::{HashMap, HashSet};

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use poe_data::views::{LeagueFlagRow, league_mechanics};
use poe_store::{HardcoreDeath, League, LeagueRefusal, LeagueState, LeagueUpdate, NewLeagueFamily};
use serde::{Deserialize, Deserializer, Serialize};
use utoipa::ToSchema;

use crate::{
    ApiError, WebState,
    art::art_url,
    auth::AdminAuth,
    error::ApiJson,
    leagues::{end_family, family_flags, unix_now},
    public::blocking,
};

fn no_data() -> ApiError {
    ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "this server has no game data loaded")
}

fn refusal(e: LeagueRefusal) -> ApiError {
    let status = match e {
        LeagueRefusal::NotFound(_) => StatusCode::NOT_FOUND,
        LeagueRefusal::Exists(_) | LeagueRefusal::HasCharacters(_) | LeagueRefusal::Ended => {
            StatusCode::CONFLICT
        }
        _ => StatusCode::BAD_REQUEST,
    };
    ApiError::new(status, e.to_string())
}

fn hc_death(v: Option<&str>) -> Result<Option<HardcoreDeath>, ApiError> {
    v.map(|t| HardcoreDeath::parse(t).ok_or_else(|| ApiError::bad_request("hc_death must be softcore or stay")))
        .transpose()
}

/// Absent field: `None`; `null`: `Some(None)`; a value: `Some(Some(v))`.
fn nullable<'de, T: Deserialize<'de>, D: Deserializer<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

/// Art of a `LeagueFlag` row as URLs (`null` when the row has none).
#[derive(Debug, Clone, Default, Serialize, ToSchema)]
pub struct FlagArt {
    /// Login-screen banner.
    pub banner_url: Option<String>,
    /// Small icon.
    pub icon_url: Option<String>,
    /// Character-creation banner icon.
    pub creation_url: Option<String>,
    /// League logo.
    pub logo_url: Option<String>,
}

impl FlagArt {
    fn of(r: &LeagueFlagRow) -> Self {
        Self {
            banner_url: art_url(&r.banner),
            icon_url: art_url(&r.image),
            creation_url: art_url(&r.banner_art),
            logo_url: art_url(&r.logo_art),
        }
    }

    /// The image shown for a league in a list: the login banner, else the others.
    fn best(&self) -> Option<String> {
        [&self.banner_url, &self.creation_url, &self.icon_url, &self.logo_url]
            .into_iter()
            .find_map(|u| u.clone())
    }
}

/// Art per `LeagueFlag.Id` (empty without game data or when the table cannot be read).
fn flag_art(s: &WebState) -> HashMap<String, FlagArt> {
    let Some(data) = &s.data else { return HashMap::new() };
    LeagueFlagRow::all(data)
        .map(|rows| rows.iter().map(|r| (r.id.clone(), FlagArt::of(r))).collect())
        .unwrap_or_default()
}

/// A league in the public list.
#[derive(Debug, Serialize, ToSchema)]
pub struct PublicLeague {
    pub id: String,
    pub display_name: String,
    /// The base league's id.
    pub family: String,
    pub hardcore: bool,
    pub ssf: bool,
    pub permanent: bool,
    pub is_default: bool,
    /// Unix seconds.
    pub starts_at: Option<i64>,
    pub ends_at: Option<i64>,
    /// `LeagueFlag.Id` of the banner.
    pub flag: Option<String>,
    /// `/api/v1/art/...` URL of the banner; null without a flag or game data.
    pub banner_url: Option<String>,
}

/// A league in the admin list.
#[derive(Debug, Serialize, ToSchema)]
pub struct AdminLeague {
    pub id: String,
    pub display_name: String,
    pub family: String,
    pub hardcore: bool,
    pub ssf: bool,
    pub permanent: bool,
    pub is_default: bool,
    pub starts_at: Option<i64>,
    pub ends_at: Option<i64>,
    pub flag: Option<String>,
    /// `active`, `hidden` or `ended`.
    pub state: String,
    pub characters: u32,
    /// `CoreLeagues` ids (stored only; none is implemented).
    pub mechanics: Vec<String>,
    /// The family characters move to when this one ends.
    pub migrate_to: Option<String>,
    pub sort: i64,
    pub created_at: i64,
    pub art: FlagArt,
    /// What a hardcore death does here: `softcore` (the character moves to the softcore
    /// league) or `stay`. Stored now; used once death exists.
    pub hc_death: String,
}

fn public(l: League, art: &HashMap<String, FlagArt>) -> PublicLeague {
    let banner_url = l.flag.as_ref().and_then(|f| art.get(f)).and_then(FlagArt::best);
    PublicLeague {
        id: l.id,
        display_name: l.display_name,
        family: l.family,
        hardcore: l.hardcore,
        ssf: l.ssf,
        permanent: l.permanent,
        is_default: l.is_default,
        starts_at: l.starts_at,
        ends_at: l.ends_at,
        flag: l.flag,
        banner_url,
    }
}

fn admin_view(l: League, counts: &HashMap<String, u32>, art: &HashMap<String, FlagArt>) -> AdminLeague {
    AdminLeague {
        characters: counts.get(&l.id).copied().unwrap_or(0),
        art: l.flag.as_ref().and_then(|f| art.get(f)).cloned().unwrap_or_default(),
        state: l.state.as_str().into(),
        id: l.id,
        display_name: l.display_name,
        family: l.family,
        hardcore: l.hardcore,
        ssf: l.ssf,
        permanent: l.permanent,
        is_default: l.is_default,
        starts_at: l.starts_at,
        ends_at: l.ends_at,
        flag: l.flag,
        mechanics: l.mechanics,
        migrate_to: l.migrate_to,
        sort: l.sort,
        created_at: l.created_at,
        hc_death: l.hc_death.as_str().into(),
    }
}

/// The active leagues, defaults first as the store orders them.
#[utoipa::path(get, operation_id = "list_leagues", path = "/api/v1/leagues", tag = "public",
    responses((status = 200, body = Vec<PublicLeague>)))]
pub async fn list(State(s): State<WebState>) -> Result<Json<Vec<PublicLeague>>, ApiError> {
    let store = s.leagues.clone();
    let rows = blocking(move || store.leagues()).await??;
    let art = flag_art(&s);
    Ok(Json(
        rows.into_iter()
            .filter(|l| l.state == LeagueState::Active)
            .map(|l| public(l, &art))
            .collect(),
    ))
}

/// Every league with its state, character count and art.
#[utoipa::path(get, operation_id = "admin_list_leagues", path = "/api/v1/admin/leagues", tag = "admin", security(("bearer" = [])),
    responses((status = 200, body = Vec<AdminLeague>), (status = 401, body = crate::ErrorBody), (status = 403, body = crate::ErrorBody)))]
pub async fn admin_list(State(s): State<WebState>, _: AdminAuth) -> Result<Json<Vec<AdminLeague>>, ApiError> {
    let store = s.leagues.clone();
    let (rows, counts) = blocking(move || Ok::<_, poe_store::StoreError>((store.leagues()?, store.character_counts()?)))
        .await??;
    let art = flag_art(&s);
    Ok(Json(rows.into_iter().map(|l| admin_view(l, &counts, &art)).collect()))
}

/// `POST /admin/leagues`: a new family.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateLeagueRequest {
    /// The base league's id and display name.
    pub name: String,
    /// Also create "HC <name>".
    #[serde(default)]
    pub hardcore: bool,
    /// Also create the SSF variants ("SSF <name>", and "HC SSF <name>" with `hardcore`).
    #[serde(default)]
    pub ssf: bool,
    /// A permanent family (no end; a target for ended leagues).
    #[serde(default)]
    pub permanent: bool,
    /// `LeagueFlag.Id` of the base; the variants get the set's own banners.
    pub flag: Option<String>,
    /// `CoreLeagues` ids.
    #[serde(default)]
    pub mechanics: Vec<String>,
    /// Unix seconds; absent: open now.
    pub starts_at: Option<i64>,
    /// Unix seconds; absent: never.
    pub ends_at: Option<i64>,
    /// Family characters move to when this one ends.
    pub migrate_to: Option<String>,
    /// Make this the default family.
    #[serde(default)]
    pub default: bool,
    /// `softcore` (default) or `stay`: what a hardcore death does.
    pub hc_death: Option<String>,
}

fn check_mechanics(s: &WebState, mechanics: &[String]) -> Result<(), ApiError> {
    if mechanics.is_empty() {
        return Ok(());
    }
    let data = s.data.as_ref().ok_or_else(no_data)?;
    let known: HashSet<String> = league_mechanics(data).map_err(ApiError::internal)?.into_iter().collect();
    match mechanics.iter().find(|m| !known.contains(*m)) {
        Some(m) => Err(ApiError::bad_request(format!("unknown league mechanic {m:?}"))),
        None => Ok(()),
    }
}

fn check_flag(s: &WebState, flag: &str) -> Result<(), ApiError> {
    let data = s.data.as_ref().ok_or_else(no_data)?;
    let rows = LeagueFlagRow::all(data).map_err(ApiError::internal)?;
    if rows.iter().any(|r| r.id == flag) {
        Ok(())
    } else {
        Err(ApiError::bad_request(format!("no LeagueFlag {flag:?}")))
    }
}

/// Creates a family: the base and the variants the flags ask for. `400` for a bad name, dates,
/// unknown flag or mechanic or a bad `migrate_to`; `409` when a name is taken.
#[utoipa::path(post, operation_id = "create_league", path = "/api/v1/admin/leagues", tag = "admin", security(("bearer" = [])),
    request_body = CreateLeagueRequest,
    responses((status = 201, body = Vec<AdminLeague>), (status = 400, body = crate::ErrorBody), (status = 409, body = crate::ErrorBody),
        (status = 503, body = crate::ErrorBody), (status = 401, body = crate::ErrorBody), (status = 403, body = crate::ErrorBody)))]
pub async fn create(
    State(s): State<WebState>,
    AdminAuth(who): AdminAuth,
    ApiJson(req): ApiJson<CreateLeagueRequest>,
) -> Result<(StatusCode, Json<Vec<AdminLeague>>), ApiError> {
    check_mechanics(&s, &req.mechanics)?;
    let flags = match &req.flag {
        Some(f) => {
            let data = s.data.as_ref().ok_or_else(no_data)?;
            family_flags(data, f).map_err(|e| ApiError::bad_request(e.to_string()))?
        }
        None => HashMap::new(),
    };
    let new = NewLeagueFamily {
        name: req.name,
        hardcore: req.hardcore,
        ssf: req.ssf,
        permanent: req.permanent,
        flags,
        mechanics: req.mechanics,
        starts_at: req.starts_at,
        ends_at: req.ends_at,
        migrate_to: req.migrate_to,
        is_default: req.default,
        hc_death: hc_death(req.hc_death.as_deref())?.unwrap_or_default(),
    };
    let store = s.leagues.clone();
    let rows = blocking(move || store.create_family(&new, unix_now())).await??.map_err(refusal)?;
    let ids: Vec<&str> = rows.iter().map(|l| l.id.as_str()).collect();
    s.admin.record(&who.account.username, "league create", &ids.join(", "));
    let art = flag_art(&s);
    let counts = HashMap::new();
    Ok((StatusCode::CREATED, Json(rows.into_iter().map(|l| admin_view(l, &counts, &art)).collect())))
}

/// `POST /admin/leagues/{id}`: absent fields stay, `null` clears the nullable ones.
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateLeagueRequest {
    pub display_name: Option<String>,
    /// `LeagueFlag.Id`; `null` clears.
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<String>)]
    pub flag: Option<Option<String>>,
    /// `active` or `hidden` (ending is `POST /admin/leagues/{id}/end`).
    pub state: Option<String>,
    /// Family-wide.
    pub mechanics: Option<Vec<String>>,
    /// Family-wide.
    pub permanent: Option<bool>,
    /// Family-wide, unix seconds; `null` clears.
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<i64>)]
    pub starts_at: Option<Option<i64>>,
    /// Family-wide, unix seconds; `null` clears.
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<i64>)]
    pub ends_at: Option<Option<i64>>,
    /// Family-wide; `null` clears.
    #[serde(default, deserialize_with = "nullable")]
    #[schema(value_type = Option<String>)]
    pub migrate_to: Option<Option<String>>,
    /// Family-wide.
    pub default: Option<bool>,
    /// Family-wide: `softcore` or `stay`.
    pub hc_death: Option<String>,
}

/// Changes a league. `400` for a bad value, `404` unknown league, `409` an ended league.
#[utoipa::path(post, operation_id = "update_league", path = "/api/v1/admin/leagues/{id}", tag = "admin", security(("bearer" = [])),
    params(("id" = String, Path, description = "league id")), request_body = UpdateLeagueRequest,
    responses((status = 200, body = AdminLeague), (status = 400, body = crate::ErrorBody), (status = 404, body = crate::ErrorBody),
        (status = 409, body = crate::ErrorBody), (status = 503, body = crate::ErrorBody), (status = 401, body = crate::ErrorBody), (status = 403, body = crate::ErrorBody)))]
pub async fn update(
    State(s): State<WebState>,
    AdminAuth(who): AdminAuth,
    Path(id): Path<String>,
    ApiJson(req): ApiJson<UpdateLeagueRequest>,
) -> Result<Json<AdminLeague>, ApiError> {
    if let Some(mechanics) = &req.mechanics {
        check_mechanics(&s, mechanics)?;
    }
    if let Some(Some(flag)) = &req.flag {
        check_flag(&s, flag)?;
    }
    let state = match req.state.as_deref() {
        None => None,
        Some(t) => match LeagueState::parse(t) {
            Some(st @ (LeagueState::Active | LeagueState::Hidden)) => Some(st),
            _ => return Err(refusal(LeagueRefusal::BadState)),
        },
    };
    let update = LeagueUpdate {
        display_name: req.display_name,
        flag: req.flag,
        state,
        mechanics: req.mechanics,
        permanent: req.permanent,
        starts_at: req.starts_at,
        ends_at: req.ends_at,
        migrate_to: req.migrate_to,
        is_default: req.default,
        hc_death: hc_death(req.hc_death.as_deref())?,
    };
    let store = s.leagues.clone();
    let league_id = id.clone();
    let (league, counts) = blocking(move || {
        let r = store.update_league(&league_id, &update)?;
        Ok::<_, poe_store::StoreError>((r, store.character_counts()?))
    })
    .await??;
    let league = league.map_err(refusal)?;
    s.admin.record(&who.account.username, "league update", &id);
    Ok(Json(admin_view(league, &counts, &flag_art(&s))))
}

/// `POST /admin/leagues/{id}/end`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct EndLeagueRequest {
    /// The family (a league id) the characters move to.
    pub target: String,
}

/// One character moved when a family ended.
#[derive(Debug, Serialize, ToSchema)]
pub struct LeagueMoveView {
    pub character_id: u32,
    pub name: String,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct EndLeagueResponse {
    /// The base id of the ended family.
    pub family: String,
    pub target: String,
    pub moved: Vec<LeagueMoveView>,
    pub count: usize,
}

/// Ends the family `{id}` belongs to and moves its characters into `target`. `400` for a bad
/// target, `404` unknown league, `409` already ended.
#[utoipa::path(post, operation_id = "end_league", path = "/api/v1/admin/leagues/{id}/end", tag = "admin", security(("bearer" = [])),
    params(("id" = String, Path, description = "any league id of the family")), request_body = EndLeagueRequest,
    responses((status = 200, body = EndLeagueResponse), (status = 400, body = crate::ErrorBody), (status = 404, body = crate::ErrorBody),
        (status = 409, body = crate::ErrorBody), (status = 401, body = crate::ErrorBody), (status = 403, body = crate::ErrorBody)))]
pub async fn end(
    State(s): State<WebState>,
    AdminAuth(who): AdminAuth,
    Path(id): Path<String>,
    ApiJson(req): ApiJson<EndLeagueRequest>,
) -> Result<Json<EndLeagueResponse>, ApiError> {
    let store = s.leagues.clone();
    let by = who.account.username.clone();
    let target = req.target;
    let t = target.clone();
    let (moves, family) = blocking(move || {
        let Some(league) = store.league(&id)? else {
            return Ok((Err(LeagueRefusal::NotFound(id)), String::new()));
        };
        let family = league.family;
        let r = end_family(&*store, &family, &t, &by)?;
        Ok::<_, anyhow::Error>((r, family))
    })
    .await?
    .map_err(ApiError::internal)?;
    let moves = moves.map_err(refusal)?;
    s.admin.record(&who.account.username, "league end", &format!("{family} -> {target}: {} moved", moves.len()));
    let moved: Vec<LeagueMoveView> = moves
        .into_iter()
        .map(|m| LeagueMoveView { character_id: m.character_id, name: m.name, from: m.from, to: m.to })
        .collect();
    Ok(Json(EndLeagueResponse { family, target, count: moved.len(), moved }))
}

/// `POST /admin/characters/{id}/league`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct MoveCharacterRequest {
    /// The league id to move the character into (any league that has not ended).
    pub league: String,
}

/// Moves a character into another league, whatever its flags (an admin's move; players leave
/// SSF or hardcore in game). `409` while the character is online, in the target's league
/// already or the target has ended, `404` for an unknown character or league.
#[utoipa::path(post, operation_id = "move_character", path = "/api/v1/admin/characters/{id}/league", tag = "admin", security(("bearer" = [])),
    params(("id" = u32, Path, description = "character id")), request_body = MoveCharacterRequest,
    responses((status = 200, body = LeagueMoveView), (status = 404, body = crate::ErrorBody),
        (status = 409, body = crate::ErrorBody), (status = 401, body = crate::ErrorBody), (status = 403, body = crate::ErrorBody)))]
pub async fn move_character(
    State(s): State<WebState>,
    AdminAuth(who): AdminAuth,
    Path(id): Path<u32>,
    ApiJson(req): ApiJson<MoveCharacterRequest>,
) -> Result<Json<LeagueMoveView>, ApiError> {
    let state = s.clone();
    let by = who.account.username.clone();
    let to = req.league;
    let m = blocking(move || -> Result<LeagueMoveView, ApiError> {
        let c = state.store.character_by_id(id)?.ok_or_else(|| ApiError::not_found("no such character"))?;
        if state.admin.online().iter().any(|o| o.character.eq_ignore_ascii_case(&c.name)) {
            return Err(ApiError::new(StatusCode::CONFLICT, format!("{} is online: ask them to log out first", c.name)));
        }
        let m = crate::leagues::move_character(&*state.leagues, id, &to, &by)?.map_err(refusal)?;
        Ok(LeagueMoveView { character_id: m.character_id, name: m.name, from: m.from, to: m.to })
    })
    .await??;
    s.admin.record(&who.account.username, "character league", &format!("{}: {} -> {}", m.name, m.from, m.to));
    Ok(Json(m))
}

/// `POST /admin/leagues/{id}/delete`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct DeleteLeagueRequest {
    /// Must equal the league id in the path.
    pub confirm: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DeleteLeagueResponse {
    /// The ids deleted: the league, or its whole family for a base.
    pub deleted: Vec<String>,
}

/// Deletes a variant, or a whole family for a base league; only while no character is in it.
/// `400` when `confirm` is not the id, `404` unknown, `409` with characters.
#[utoipa::path(post, operation_id = "delete_league", path = "/api/v1/admin/leagues/{id}/delete", tag = "admin", security(("bearer" = [])),
    params(("id" = String, Path, description = "league id")), request_body = DeleteLeagueRequest,
    responses((status = 200, body = DeleteLeagueResponse), (status = 400, body = crate::ErrorBody), (status = 404, body = crate::ErrorBody),
        (status = 409, body = crate::ErrorBody), (status = 401, body = crate::ErrorBody), (status = 403, body = crate::ErrorBody)))]
pub async fn delete(
    State(s): State<WebState>,
    AdminAuth(who): AdminAuth,
    Path(id): Path<String>,
    ApiJson(req): ApiJson<DeleteLeagueRequest>,
) -> Result<Json<DeleteLeagueResponse>, ApiError> {
    if req.confirm != id {
        return Err(ApiError::bad_request("confirm must be the league id"));
    }
    let store = s.leagues.clone();
    let league_id = id.clone();
    let deleted = blocking(move || store.delete_league(&league_id)).await??.map_err(refusal)?;
    s.admin.record(&who.account.username, "league delete", &deleted.join(", "));
    Ok(Json(DeleteLeagueResponse { deleted }))
}

/// A `LeagueFlag` row.
#[derive(Debug, Serialize, ToSchema)]
pub struct LeagueFlagView {
    pub row: u32,
    pub id: String,
    pub hardcore: bool,
    pub ssf: bool,
    pub ruthless: bool,
    pub art: FlagArt,
}

/// The `LeagueFlag` rows (banners to pick from).
#[utoipa::path(get, operation_id = "league_flags", path = "/api/v1/admin/data/league-flags", tag = "admin", security(("bearer" = [])),
    responses((status = 200, body = Vec<LeagueFlagView>), (status = 503, body = crate::ErrorBody), (status = 401, body = crate::ErrorBody), (status = 403, body = crate::ErrorBody)))]
pub async fn flags(State(s): State<WebState>, _: AdminAuth) -> Result<Json<Vec<LeagueFlagView>>, ApiError> {
    let data = s.data.as_ref().ok_or_else(no_data)?;
    let rows = LeagueFlagRow::all(data).map_err(ApiError::internal)?;
    Ok(Json(
        rows.iter()
            .map(|r| LeagueFlagView {
                row: r.row,
                id: r.id.clone(),
                hardcore: r.hardcore,
                ssf: r.ssf,
                ruthless: r.ruthless,
                art: FlagArt::of(r),
            })
            .collect(),
    ))
}

/// A league mechanic (`CoreLeagues` row).
#[derive(Debug, Serialize, ToSchema)]
pub struct MechanicView {
    pub id: String,
    /// The id split into words.
    pub name: String,
    /// Always false for now: leagues store mechanics, none is implemented.
    pub implemented: bool,
}

/// "RogueExiles" -> "Rogue Exiles", "Expedition2" -> "Expedition 2".
pub fn mechanic_name(id: &str) -> String {
    let mut out = String::new();
    let mut prev: Option<char> = None;
    for c in id.chars() {
        let split = prev.is_some_and(|p| {
            (c.is_uppercase() && (p.is_lowercase() || p.is_ascii_digit()))
                || (c.is_ascii_digit() && p.is_alphabetic())
        });
        if split {
            out.push(' ');
        }
        out.push(c);
        prev = Some(c);
    }
    out
}

/// The league mechanics of the game data.
#[utoipa::path(get, operation_id = "league_mechanics", path = "/api/v1/admin/data/league-mechanics", tag = "admin", security(("bearer" = [])),
    responses((status = 200, body = Vec<MechanicView>), (status = 503, body = crate::ErrorBody), (status = 401, body = crate::ErrorBody), (status = 403, body = crate::ErrorBody)))]
pub async fn mechanics(State(s): State<WebState>, _: AdminAuth) -> Result<Json<Vec<MechanicView>>, ApiError> {
    let data = s.data.as_ref().ok_or_else(no_data)?;
    let ids = league_mechanics(data).map_err(ApiError::internal)?;
    Ok(Json(
        ids.into_iter()
            .map(|id| MechanicView { name: mechanic_name(&id), id, implemented: false })
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mechanic_names() {
        assert_eq!(mechanic_name("RogueExiles"), "Rogue Exiles");
        assert_eq!(mechanic_name("Expedition2"), "Expedition 2");
        assert_eq!(mechanic_name("Breach"), "Breach");
    }

    #[test]
    fn nullable_fields() {
        let r: UpdateLeagueRequest = serde_json::from_str(r#"{"flag": null, "ends_at": 5}"#).unwrap();
        assert_eq!((r.flag, r.ends_at, r.starts_at), (Some(None), Some(Some(5)), None));
    }
}
