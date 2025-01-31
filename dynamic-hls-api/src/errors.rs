use crate::mpegts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Error, Debug)]
#[error(transparent)]
pub struct AppError(Box<ErrorKind>);

#[derive(Error, Debug)]
#[error(transparent)]
pub enum ErrorKind {
    #[error("SerdeJsonError: {0}")]
    SerdeJsonError(#[from] serde_json::Error),
    #[error("IoError: {0}")]
    IoError(#[from] std::io::Error),
    #[error("Mp4Error: {0}")]
    Mp4Error(#[from] mp4::Error),
    #[error("TsError: {0}")]
    TsError(#[from] mpegts::TsError),
}

impl<E> From<E> for AppError
where
    ErrorKind: From<E>,
{
    fn from(err: E) -> Self {
        AppError(Box::new(ErrorKind::from(err)))
    }
}

impl AppError {
    fn get_codes(&self) -> (StatusCode, u16) {
        match *self.0 {
            ErrorKind::SerdeJsonError(_) => (StatusCode::BAD_REQUEST, 40001),
            ErrorKind::IoError(_) => (StatusCode::BAD_REQUEST, 40002),
            ErrorKind::Mp4Error(_) => (StatusCode::BAD_REQUEST, 40003),
            ErrorKind::TsError(_) => (StatusCode::BAD_REQUEST, 40004),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct ErrorCode {
    pub code: u16,
    pub message: String,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status_code, code) = self.get_codes();
        let message = self.to_string();
        let body = Json(ErrorCode { code, message });
        (status_code, body).into_response()
    }
}
pub type Result<T> = std::result::Result<T, AppError>;
