//! Errors as RFC 9457 problem details (`application/problem+json`), plus
//! the request extractors whose rejections are reported the same way, so
//! a client never has to parse a second error format.

use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::FromRequest;
use axum::extract::FromRequestParts;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use fluxolap_error::{ErrorCode, FluxolapError};

pub use fluxolap_api_types::Problem;

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: String,
    detail: String,
    error_code: Option<u32>,
    index: Option<usize>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &str, detail: impl Into<String>) -> Self {
        Self { status, code: code.to_string(), detail: detail.into(), error_code: None, index: None }
    }

    pub fn unprocessable(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_request", detail)
    }

    pub fn route_not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "route_not_found", "no such resource")
    }

    /// Marks the error as caused by item `index` of a batch request.
    pub fn at(mut self, index: usize) -> Self {
        self.index = Some(index);
        self
    }

    pub fn status(&self) -> StatusCode {
        self.status
    }
}

/// HTTP status for an engine error: missing resources are 404, clashes
/// with existing state 409, requests that are well-formed but can't be
/// carried out 422.
pub fn status_for(code: ErrorCode) -> StatusCode {
    match code {
        ErrorCode::DATABASE_NOT_FOUND | ErrorCode::DIMENSION_NOT_FOUND | ErrorCode::ELEMENT_NOT_FOUND | ErrorCode::CUBE_NOT_FOUND | ErrorCode::CUBE_LOCK_NOT_FOUND => {
            StatusCode::NOT_FOUND
        }
        ErrorCode::DATABASE_NAME_IN_USE
        | ErrorCode::DIMENSION_NAME_IN_USE
        | ErrorCode::DIMENSION_IN_USE
        | ErrorCode::ELEMENT_NAME_IN_USE
        | ErrorCode::CUBE_NAME_IN_USE
        | ErrorCode::ELEMENT_CIRCULAR_REFERENCE
        | ErrorCode::CUBE_WRONG_LOCK
        | ErrorCode::CUBE_BLOCKED_BY_LOCK => StatusCode::CONFLICT,
        ErrorCode::API_CALL_NOT_IMPLEMENTED => StatusCode::NOT_IMPLEMENTED,
        ErrorCode::INTERNAL => StatusCode::INTERNAL_SERVER_ERROR,
        ErrorCode::AUTHORIZATION_FAILED => StatusCode::UNAUTHORIZED,
        _ => StatusCode::UNPROCESSABLE_ENTITY,
    }
}

impl From<FluxolapError> for ApiError {
    fn from(e: FluxolapError) -> Self {
        Self { status: status_for(e.code), code: e.error_type.replace(' ', "_"), detail: e.message, error_code: Some(e.code.0), index: None }
    }
}

impl From<JsonRejection> for ApiError {
    fn from(r: JsonRejection) -> Self {
        Self::new(r.status(), "invalid_body", r.body_text())
    }
}

impl From<PathRejection> for ApiError {
    fn from(r: PathRejection) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_path", r.body_text())
    }
}

impl From<QueryRejection> for ApiError {
    fn from(r: QueryRejection) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_query", r.body_text())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let problem = Problem {
            problem_type: "about:blank".to_string(),
            title: self.status.canonical_reason().unwrap_or("Error").to_string(),
            status: self.status.as_u16(),
            detail: self.detail,
            code: self.code,
            error_code: self.error_code,
            index: self.index,
        };
        let body = serde_json::to_vec(&problem).expect("problem serializes");
        (self.status, [(header::CONTENT_TYPE, "application/problem+json")], body).into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

/// `axum::Json`, with rejections reported as problem details.
#[derive(FromRequest)]
#[from_request(via(axum::Json), rejection(ApiError))]
pub struct Json<T>(pub T);

/// `axum::extract::Path`, with rejections reported as problem details.
#[derive(FromRequestParts)]
#[from_request(via(axum::extract::Path), rejection(ApiError))]
pub struct Path<T>(pub T);

/// `axum::extract::Query`, with rejections reported as problem details.
#[derive(FromRequestParts)]
#[from_request(via(axum::extract::Query), rejection(ApiError))]
pub struct Query<T>(pub T);
