//! Object storage handlers.

use axum::{
    body::Body,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use dcfs_core::ObjectId;
use dcfs_objectstore::ObjectLocator;
use uuid::Uuid;

use crate::{error::AppError, state::AppState};

/// Get an object's data.
pub async fn get_object(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let object_id = ObjectId::from_uuid(id);
    let locator = ObjectLocator::new(object_id);
    let data: bytes::Bytes = state.store.get(&locator).await?;
    Ok((StatusCode::OK, Body::from(data)))
}
