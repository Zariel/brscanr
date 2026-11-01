//! Native Brother scanner protocol for the MFC-L2800DW family.
//!
//! Bind a notification listener before registering a callback. Serialize all
//! acquisitions for a device; an interrupted acquisition must never be retried
//! automatically because paper might already have moved.

mod acquisition;
mod document;
mod notification;
mod registration;

pub use acquisition::{Capture, Limits, Page, Scanner, Source};
pub use document::assemble_pdf;
pub use notification::Notification;
pub use registration::Registration;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("scanner I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("scanner HTTP: {0}")]
    Http(#[from] reqwest::Error),
    #[error("invalid scanner protocol: {0}")]
    Protocol(String),
    #[error("scanner operation timed out")]
    Timeout,
    #[error("scan exceeds {0} limit")]
    Limit(&'static str),
    #[error("feeder is empty")]
    EmptyFeeder,
    #[error("document jam")]
    Jam,
}

pub type Result<T> = std::result::Result<T, Error>;

fn protocol(message: impl Into<String>) -> Error {
    Error::Protocol(message.into())
}
