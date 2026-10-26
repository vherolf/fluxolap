//! Elements, batch-first: `POST` creates, `PUT` creates-or-updates by
//! name, `PATCH` updates, and `POST .../elements/delete` deletes many
//! elements in one request (a JSON body, since tens of thousands of ids
//! don't fit in a URL). Every batch is all-or-nothing: it runs against a copy of the
//! dimension that replaces the real one only if every item succeeded
//! (the copy costs time proportional to the dimension's size, once per
//! request). An error names the failing item's `index`.

use crate::dto::{ElementClearQuery, ElementDeleteBatch, ElementDto, ElementList, ElementPatch, ElementQuery, ElementType, ElementUpdateBatch, ElementWrite, ElementWriteBatch};
use crate::error::{ApiError, ApiResult, Json, Path, Query};
use crate::resolve::{self, database, dimension_id, element_id, element_ref, Db};
use axum::extract::State;
use axum::http::StatusCode;
use fluxolap_domain::dimension::{Dimension, DimensionTable};
use fluxolap_domain::{DimensionId, ElementId};
use fluxolap_engine::{ops, AppState};
use std::collections::HashSet;

fn element_error(e: fluxolap_domain::DomainError) -> ApiError {
    ops::map_element_error(e).into()
}

/// Runs `f` on a copy of the dimension and stores the copy only if `f`
/// succeeds, then returns `f`'s elements as DTOs.
fn with_dimension_copy(db: &Db, dim: u32, f: impl FnOnce(&mut Dimension) -> ApiResult<Vec<ElementId>>) -> ApiResult<Vec<ElementDto>> {
    let mut dimensions = db.loaded.dimensions.write().expect("dimensions lock poisoned");
    let dim_id = dimension_id(&dimensions, db.id, dim)?;
    let mut work = dimensions.get(dim_id).expect("just found").clone();
    let ids = f(&mut work)?;
    let items = ids.iter().map(|&id| ElementDto::new(&work, id)).collect();
    *dimensions.get_mut(dim_id).expect("just found") = work;
    drop(dimensions);
    db.loaded.mark_dirty();
    Ok(items)
}

/// Creates (or, with `upsert`, creates-or-updates by name) every item,
/// then sets children in a second pass so an item's children can be
/// elements created later in the same batch.
fn write_batch(dim: &mut Dimension, items: &[ElementWrite], upsert: bool) -> ApiResult<Vec<ElementId>> {
    let mut ids = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let has_children = item.children.as_ref().is_some_and(|c| !c.is_empty());
        let existing = if upsert { dim.find_element_by_name(&item.name) } else { None };
        let id = match existing {
            Some(id) => {
                // A consolidated element asked to be `numeric` stays as it is (the
                // engine treats that as a no-op), so re-sending a dimension is harmless.
                if !has_children && item.elem_type != ElementType::Consolidated {
                    dim.change_element_type(id, item.elem_type.raw()).map_err(|e| element_error(e).at(i))?;
                }
                id
            }
            None => {
                // Consolidated elements start as numeric leaves and turn consolidated
                // when given children (a childless `consolidated` item stays a leaf).
                let raw = if has_children || item.elem_type == ElementType::Consolidated { ElementType::Numeric.raw() } else { item.elem_type.raw() };
                dim.create_element(&item.name, raw).map_err(|e| element_error(e).at(i))?
            }
        };
        ids.push(id);
    }
    for (i, (item, &id)) in items.iter().zip(&ids).enumerate() {
        if let Some(children) = &item.children {
            let pairs = resolve::children(dim, children).map_err(|e| e.at(i))?;
            dim.set_children(id, &pairs).map_err(|e| element_error(e).at(i))?;
        }
    }
    Ok(ids)
}

fn apply_patch(dim: &mut Dimension, id: ElementId, patch: &ElementPatch) -> ApiResult<()> {
    if let Some(name) = &patch.name {
        dim.rename_element(id, name).map_err(element_error)?;
    }
    if let Some(children) = &patch.children {
        let pairs = resolve::children(dim, children)?;
        dim.set_children(id, &pairs).map_err(element_error)?;
    }
    if let Some(children) = &patch.add_children {
        let pairs = resolve::children(dim, children)?;
        dim.add_children(id, &pairs).map_err(element_error)?;
    }
    if let Some(elem_type) = patch.elem_type {
        let consolidated = !dim.children_of(id).is_empty();
        match elem_type {
            ElementType::Consolidated if !consolidated => return Err(ApiError::unprocessable("an element becomes consolidated by being given children")),
            ElementType::Consolidated => {}
            _ if consolidated => return Err(ApiError::unprocessable("a consolidated element can't change type while it has children")),
            _ => dim.change_element_type(id, elem_type.raw()).map_err(element_error)?,
        }
    }
    if let Some(position) = patch.position {
        dim.move_element(id, position).map_err(element_error)?;
    }
    Ok(())
}

/// Deletes elements, first clearing the stored cells of the leaf ones in
/// every cube that uses the dimension (consolidated elements have no
/// stored cells), so no cell is left pointing at a deleted element.
fn delete_elements(db: &Db, dimensions: &mut DimensionTable, dim_id: DimensionId, ids: &[ElementId]) {
    let dim = dimensions.get(dim_id).expect("caller resolved it");
    let leaves: Vec<ElementId> = ids.iter().copied().filter(|&id| dim.children_of(id).is_empty()).collect();
    if !leaves.is_empty() {
        let cubes = db.loaded.cubes.read().expect("cubes lock poisoned");
        for (cube_id, cube) in cubes.list_in_database(db.id) {
            if let Some(d) = cube.dimensions.iter().position(|&x| x == dim_id) {
                let mut area = vec![None; cube.dimensions.len()];
                area[d] = Some(leaves.clone());
                ops::clear_area(dimensions, &db.loaded.storage, cube_id, cube, &area);
            }
        }
    }
    dimensions.get_mut(dim_id).expect("caller resolved it").destroy_elements(ids).expect("ids were resolved");
}

// ---------------------------------------------------------------------------
// Collection
// ---------------------------------------------------------------------------

/// Lists elements in position order.
#[utoipa::path(get, path = "/api/v1/databases/{db}/dimensions/{dim}/elements", tag = "elements",
    params(("db" = u32, Path, description = "Database id"), ("dim" = u32, Path, description = "Dimension id"), ElementQuery),
    responses((status = 200, body = ElementList), (status = 404, body = crate::error::Problem, content_type = "application/problem+json")))]
pub async fn list(State(state): State<AppState>, Path((db, dim)): Path<(u32, u32)>, Query(q): Query<ElementQuery>) -> ApiResult<axum::Json<ElementList>> {
    let db = database(&state, db)?;
    let dimensions = db.loaded.dimensions.read().expect("dimensions lock poisoned");
    let dim = dimensions.get(dimension_id(&dimensions, db.id, dim)?).expect("just found");
    let mut ids: Vec<(u32, ElementId)> = dim
        .list_elements()
        .filter(|(_, el)| q.name.as_ref().is_none_or(|n| *n == el.name))
        .filter(|(_, el)| q.elem_type.is_none_or(|t| t == el.elem_type.into()))
        .filter(|&(id, _)| !q.roots || dim.parents_of(id).is_empty())
        .map(|(id, el)| (el.position, id))
        .collect();
    ids.sort_unstable_by_key(|&(position, _)| position);
    let total = ids.len();
    let items = ids.into_iter().skip(q.offset).take(q.limit.unwrap_or(usize::MAX)).map(|(_, id)| ElementDto::new(dim, id)).collect();
    Ok(axum::Json(ElementList { items, total }))
}

/// Creates a batch of elements. Children may name elements created
/// earlier or later in the same batch. Fails (409) on an existing name.
#[utoipa::path(post, path = "/api/v1/databases/{db}/dimensions/{dim}/elements", tag = "elements",
    params(("db" = u32, Path, description = "Database id"), ("dim" = u32, Path, description = "Dimension id")), request_body = ElementWriteBatch,
    responses(
        (status = 201, body = ElementList, description = "The created elements, in request order"),
        (status = 404, body = crate::error::Problem, content_type = "application/problem+json"),
        (status = 409, body = crate::error::Problem, content_type = "application/problem+json"),
        (status = 422, body = crate::error::Problem, content_type = "application/problem+json"),
    ))]
pub async fn create(State(state): State<AppState>, Path((db, dim)): Path<(u32, u32)>, Json(batch): Json<ElementWriteBatch>) -> ApiResult<(StatusCode, axum::Json<ElementList>)> {
    let db = database(&state, db)?;
    let items = with_dimension_copy(&db, dim, |d| write_batch(d, &batch.items, false))?;
    Ok((StatusCode::CREATED, axum::Json(ElementList { total: items.len(), items })))
}

/// Creates-or-updates a batch of elements by name -- idempotent, for
/// (re)loading a dimension. An existing element takes the item's `type`;
/// given `children` replace its child list, omitted ones keep it.
#[utoipa::path(put, path = "/api/v1/databases/{db}/dimensions/{dim}/elements", tag = "elements",
    params(("db" = u32, Path, description = "Database id"), ("dim" = u32, Path, description = "Dimension id")), request_body = ElementWriteBatch,
    responses(
        (status = 200, body = ElementList, description = "The written elements, in request order"),
        (status = 404, body = crate::error::Problem, content_type = "application/problem+json"),
        (status = 409, body = crate::error::Problem, content_type = "application/problem+json"),
        (status = 422, body = crate::error::Problem, content_type = "application/problem+json"),
    ))]
pub async fn upsert(State(state): State<AppState>, Path((db, dim)): Path<(u32, u32)>, Json(batch): Json<ElementWriteBatch>) -> ApiResult<axum::Json<ElementList>> {
    let db = database(&state, db)?;
    let items = with_dimension_copy(&db, dim, |d| write_batch(d, &batch.items, true))?;
    Ok(axum::Json(ElementList { total: items.len(), items }))
}

/// Updates a batch of elements (rename, retype, move, change children),
/// applied in order.
#[utoipa::path(patch, path = "/api/v1/databases/{db}/dimensions/{dim}/elements", tag = "elements",
    params(("db" = u32, Path, description = "Database id"), ("dim" = u32, Path, description = "Dimension id")), request_body = ElementUpdateBatch,
    responses(
        (status = 200, body = ElementList, description = "The updated elements, in request order"),
        (status = 404, body = crate::error::Problem, content_type = "application/problem+json"),
        (status = 409, body = crate::error::Problem, content_type = "application/problem+json"),
        (status = 422, body = crate::error::Problem, content_type = "application/problem+json"),
    ))]
pub async fn update_many(State(state): State<AppState>, Path((db, dim)): Path<(u32, u32)>, Json(batch): Json<ElementUpdateBatch>) -> ApiResult<axum::Json<ElementList>> {
    let db = database(&state, db)?;
    let items = with_dimension_copy(&db, dim, |d| {
        let mut ids = Vec::with_capacity(batch.items.len());
        for (i, update) in batch.items.into_iter().enumerate() {
            let (r, patch) = update.split();
            let id = element_ref(d, &r).map_err(|e| e.at(i))?;
            apply_patch(d, id, &patch).map_err(|e| e.at(i))?;
            ids.push(id);
        }
        Ok(ids)
    })?;
    Ok(axum::Json(ElementList { total: items.len(), items }))
}

/// Deletes a batch of elements, clearing the cells of the leaf ones.
/// All-or-nothing: an unknown element (404, with `index`) deletes nothing.
#[utoipa::path(post, path = "/api/v1/databases/{db}/dimensions/{dim}/elements/delete", tag = "elements",
    params(("db" = u32, Path, description = "Database id"), ("dim" = u32, Path, description = "Dimension id")), request_body = ElementDeleteBatch,
    responses((status = 204), (status = 404, body = crate::error::Problem, content_type = "application/problem+json")))]
pub async fn delete_many(State(state): State<AppState>, Path((db, dim)): Path<(u32, u32)>, Json(batch): Json<ElementDeleteBatch>) -> ApiResult<StatusCode> {
    let db = database(&state, db)?;
    let mut dimensions = db.loaded.dimensions.write().expect("dimensions lock poisoned");
    let dim_id = dimension_id(&dimensions, db.id, dim)?;
    let dim = dimensions.get(dim_id).expect("just found");
    let mut seen = HashSet::new();
    let mut ids = Vec::with_capacity(batch.items.len());
    for (i, r) in batch.items.iter().enumerate() {
        let id = element_ref(dim, r).map_err(|e| e.at(i))?;
        if seen.insert(id) {
            ids.push(id);
        }
    }
    delete_elements(&db, &mut dimensions, dim_id, &ids);
    drop(dimensions);
    db.loaded.mark_dirty();
    Ok(StatusCode::NO_CONTENT)
}

/// Deletes every element of the dimension (`all=true` required) and
/// clears every cube that uses it.
#[utoipa::path(delete, path = "/api/v1/databases/{db}/dimensions/{dim}/elements", tag = "elements",
    params(("db" = u32, Path, description = "Database id"), ("dim" = u32, Path, description = "Dimension id"), ElementClearQuery),
    responses(
        (status = 204),
        (status = 404, body = crate::error::Problem, content_type = "application/problem+json"),
        (status = 422, body = crate::error::Problem, content_type = "application/problem+json"),
    ))]
pub async fn clear(State(state): State<AppState>, Path((db, dim)): Path<(u32, u32)>, Query(q): Query<ElementClearQuery>) -> ApiResult<StatusCode> {
    if !q.all {
        return Err(ApiError::unprocessable("deleting every element needs `all=true`; use POST .../elements/delete for some"));
    }
    let db = database(&state, db)?;
    let mut dimensions = db.loaded.dimensions.write().expect("dimensions lock poisoned");
    let dim_id = dimension_id(&dimensions, db.id, dim)?;
    dimensions.get_mut(dim_id).expect("just found").clear(None);
    let cubes = db.loaded.cubes.read().expect("cubes lock poisoned");
    for (cube_id, _) in cubes.list_in_database(db.id).filter(|(_, c)| c.dimensions.contains(&dim_id)) {
        db.loaded.storage.clear_cube(cube_id);
    }
    drop(cubes);
    drop(dimensions);
    db.loaded.mark_dirty();
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Single element
// ---------------------------------------------------------------------------

/// Gets one element.
#[utoipa::path(get, path = "/api/v1/databases/{db}/dimensions/{dim}/elements/{el}", tag = "elements",
    params(("db" = u32, Path, description = "Database id"), ("dim" = u32, Path, description = "Dimension id"), ("el" = u32, Path, description = "Element id")),
    responses((status = 200, body = ElementDto), (status = 404, body = crate::error::Problem, content_type = "application/problem+json")))]
pub async fn get(State(state): State<AppState>, Path((db, dim, el)): Path<(u32, u32, u32)>) -> ApiResult<axum::Json<ElementDto>> {
    let db = database(&state, db)?;
    let dimensions = db.loaded.dimensions.read().expect("dimensions lock poisoned");
    let dim = dimensions.get(dimension_id(&dimensions, db.id, dim)?).expect("just found");
    Ok(axum::Json(ElementDto::new(dim, element_id(dim, el)?)))
}

/// Updates one element.
#[utoipa::path(patch, path = "/api/v1/databases/{db}/dimensions/{dim}/elements/{el}", tag = "elements",
    params(("db" = u32, Path, description = "Database id"), ("dim" = u32, Path, description = "Dimension id"), ("el" = u32, Path, description = "Element id")),
    request_body = ElementPatch,
    responses(
        (status = 200, body = ElementDto),
        (status = 404, body = crate::error::Problem, content_type = "application/problem+json"),
        (status = 409, body = crate::error::Problem, content_type = "application/problem+json"),
        (status = 422, body = crate::error::Problem, content_type = "application/problem+json"),
    ))]
pub async fn update(State(state): State<AppState>, Path((db, dim, el)): Path<(u32, u32, u32)>, Json(patch): Json<ElementPatch>) -> ApiResult<axum::Json<ElementDto>> {
    let db = database(&state, db)?;
    let mut items = with_dimension_copy(&db, dim, |d| {
        let id = element_id(d, el)?;
        apply_patch(d, id, &patch)?;
        Ok(vec![id])
    })?;
    Ok(axum::Json(items.pop().expect("one element")))
}

/// Deletes one element, clearing its cells if it's a leaf.
#[utoipa::path(delete, path = "/api/v1/databases/{db}/dimensions/{dim}/elements/{el}", tag = "elements",
    params(("db" = u32, Path, description = "Database id"), ("dim" = u32, Path, description = "Dimension id"), ("el" = u32, Path, description = "Element id")),
    responses((status = 204), (status = 404, body = crate::error::Problem, content_type = "application/problem+json")))]
pub async fn delete(State(state): State<AppState>, Path((db, dim, el)): Path<(u32, u32, u32)>) -> ApiResult<StatusCode> {
    let db = database(&state, db)?;
    let mut dimensions = db.loaded.dimensions.write().expect("dimensions lock poisoned");
    let dim_id = dimension_id(&dimensions, db.id, dim)?;
    let id = element_id(dimensions.get(dim_id).expect("just found"), el)?;
    delete_elements(&db, &mut dimensions, dim_id, &[id]);
    drop(dimensions);
    db.loaded.mark_dirty();
    Ok(StatusCode::NO_CONTENT)
}
