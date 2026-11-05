//! The server's errors, and how they become HTTP responses.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use kn_proto::api::ErrorBody;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Unauthorized(String),
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    NotFound(String),
    /// Refused because of what else is in the cluster, such as a domain another app uses.
    #[error("{0}")]
    Conflict(String),
    /// The node it needs is offline.
    #[error("{0}")]
    Offline(String),
    #[error("{0}")]
    TooLarge(String),
    /// A bundle that was refused, with each problem.
    #[error("{message}")]
    Refused { message: String, problems: Vec<String> },
    #[error("{0}")]
    UpgradeRequired(String),
    /// Too many failed logins.
    #[error("{0}")]
    TooManyRequests(String),
    /// The node making the request was forgotten (`node forget`): answered with `410 Gone` and a
    /// [`kn_proto::agent::ForgottenBody`], which only this answers with.
    #[error("{}", kn_proto::agent::FORGOTTEN)]
    Forgotten { node_id: u64, name: String, at: u64 },
    /// An error from a node, passed on with the status it chose.
    #[error("{}", .1.error)]
    Node(StatusCode, ErrorBody),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Internal(e.into())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Internal(e.into())
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Internal(e.into())
    }
}

impl Error {
    pub fn status(&self) -> StatusCode {
        match self {
            Error::BadRequest(_) => StatusCode::BAD_REQUEST,
            Error::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            Error::Forbidden(_) => StatusCode::FORBIDDEN,
            Error::NotFound(_) => StatusCode::NOT_FOUND,
            Error::Conflict(_) => StatusCode::CONFLICT,
            Error::Offline(_) => StatusCode::SERVICE_UNAVAILABLE,
            Error::TooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            Error::Refused { .. } => StatusCode::UNPROCESSABLE_ENTITY,
            Error::UpgradeRequired(_) => StatusCode::UPGRADE_REQUIRED,
            Error::TooManyRequests(_) => StatusCode::TOO_MANY_REQUESTS,
            Error::Forgotten { .. } => StatusCode::GONE,
            Error::Node(status, _) => *status,
            Error::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn body(&self) -> ErrorBody {
        match self {
            Error::Refused { message, problems } => ErrorBody {
                error: message.clone(),
                problems: problems.clone(),
            },
            Error::Node(_, body) => body.clone(),
            Error::Internal(e) => ErrorBody {
                error: format!("{e:#}"),
                problems: Vec::new(),
            },
            e => ErrorBody {
                error: e.to_string(),
                problems: Vec::new(),
            },
        }
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        if let Error::Forgotten { node_id, name, at } = self {
            let body = kn_proto::agent::ForgottenBody {
                error: kn_proto::agent::FORGOTTEN.into(),
                node_id,
                name,
                forgotten_at: at,
            };
            return (StatusCode::GONE, axum::Json(body)).into_response();
        }
        let status = self.status();
        if status.is_server_error() && !matches!(self, Error::Offline(_)) {
            tracing::warn!(%status, error = %self, "request failed");
        }
        (status, axum::Json(self.body())).into_response()
    }
}
