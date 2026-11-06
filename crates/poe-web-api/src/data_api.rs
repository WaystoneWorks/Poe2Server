//! Game data lists for the web admin's pickers: `GET /admin/data/areas` (`WorldAreas`).
//! Answers `503` when the server has no game data.

use std::sync::OnceLock;

use axum::{Json, extract::State, http::StatusCode};
use poe_data::views::WorldArea;
use serde::Serialize;
use utoipa::ToSchema;

use crate::{ApiError, WebState, auth::AdminAuth, public::blocking};

/// A `WorldAreas` row for the area pickers (create instance, transfer).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AreaView {
    /// `WorldAreas.Id`, e.g. "G1_1".
    pub id: String,
    /// `WorldAreas.Name`, e.g. "The Riverbank".
    pub name: String,
    pub act: i64,
    pub level: i64,
    pub is_town: bool,
    pub is_hideout: bool,
}

/// The game data of a server process never changes: the list is built once.
static AREAS: OnceLock<Vec<AreaView>> = OnceLock::new();

/// Every world area with a name, in table order.
#[utoipa::path(get, operation_id = "areas", path = "/api/v1/admin/data/areas", tag = "admin", security(("bearer" = [])),
    responses((status = 200, body = Vec<AreaView>), (status = 503, body = crate::ErrorBody), (status = 401, body = crate::ErrorBody), (status = 403, body = crate::ErrorBody)))]
pub async fn areas(State(s): State<WebState>, _: AdminAuth) -> Result<Json<Vec<AreaView>>, ApiError> {
    if let Some(list) = AREAS.get() {
        return Ok(Json(list.clone()));
    }
    let data = s.data.clone().ok_or_else(|| {
        ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "this server has no game data loaded")
    })?;
    let list = blocking(move || -> Result<Vec<AreaView>, ApiError> {
        Ok(WorldArea::all(&data)
            .map_err(ApiError::internal)?
            .into_iter()
            .filter(|a| !a.name.is_empty())
            .map(|a| AreaView {
                id: a.id,
                name: a.name,
                act: a.act,
                level: a.level,
                is_town: a.is_town,
                is_hideout: a.is_hideout,
            })
            .collect())
    })
    .await??;
    Ok(Json(AREAS.get_or_init(|| list).clone()))
}
