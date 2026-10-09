//! Ошибки API: protobuf `Error` с корректным HTTP-статусом. Внутренние детали
//! (SQL, пути ФС) пишутся в лог, но клиенту не уходят.

use axum::response::{IntoResponse, Response};
use http::{HeaderValue, StatusCode, header};
use notesync_proto::v1 as pb;
use notesync_proto::{CONTENT_TYPE_PROTOBUF, PROTO_VERSION};
use prost::Message;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        ApiError {
            status,
            code,
            message: message.into(),
        }
    }

    pub fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message)
    }

    pub fn not_found(code: &'static str) -> Self {
        Self::new(StatusCode::NOT_FOUND, code, "не найдено")
    }

    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "нужен действующий токен",
        )
    }

    pub fn conflict(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, code, message)
    }

    pub fn too_large(message: impl Into<String>) -> Self {
        Self::new(StatusCode::PAYLOAD_TOO_LARGE, "too_large", message)
    }

    /// Внутренняя ошибка: подробности — только в лог.
    pub fn internal(err: impl std::fmt::Display) -> Self {
        tracing::error!(error = %err, "внутренняя ошибка");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "внутренняя ошибка сервера",
        )
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        ApiError::internal(e)
    }
}

impl From<r2d2::Error> for ApiError {
    fn from(e: r2d2::Error) -> Self {
        ApiError::internal(e)
    }
}

impl From<std::io::Error> for ApiError {
    fn from(e: std::io::Error) -> Self {
        ApiError::internal(e)
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::internal(e)
    }
}

impl From<tokio::task::JoinError> for ApiError {
    fn from(e: tokio::task::JoinError) -> Self {
        ApiError::internal(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = pb::Error {
            code: self.code.to_owned(),
            message: self.message,
            supported_proto: if self.status == StatusCode::UPGRADE_REQUIRED {
                PROTO_VERSION
            } else {
                0
            },
        }
        .encode_to_vec();
        let mut resp = (self.status, body).into_response();
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(CONTENT_TYPE_PROTOBUF),
        );
        resp
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
