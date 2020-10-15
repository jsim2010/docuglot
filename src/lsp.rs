//! Implements the language server protocol functionality of docuglot.
// TODO: This should be moved to only occur on cur (functionality exists in master but not 1.46.0.
#![allow(clippy::wildcard_imports)] // cur is designed to use wildcard import.
use {
    crate::json_rpc::{Id, Kind, Object, Outcome, Params},
    conventus::{AssembleFailure, AssembleFrom, DisassembleFrom},
    core::{
        convert::{TryFrom, TryInto},
        fmt::Display,
    },
    fehler::{throw, throws},
    lsp_types::{
        notification::{Notification, PublishDiagnostics},
        request::{RegisterCapability, Request},
        InitializeResult, PublishDiagnosticsParams, RegistrationParams,
    },
    parse_display::Display as ParseDisplay,
    serde_json::{error::Error as SerdeJsonError, Value},
    std::str::Utf8Error,
    thiserror::Error as ThisError,
};

use cur::*;

/// The header field name that maps to the length of the content.
static HEADER_CONTENT_LENGTH: &str = "Content-Length";
/// Delimiter between a header field and value.
static HEADER_FIELD_NAME_DELIMITER: &str = ": ";
/// The end of the header.
static HEADER_END: &str = "\r\n\r\n";
/// The end of a header field.
static HEADER_FIELD_DELIMITER: &str = "\r\n";

game!(TCHAR = '!' | '#' | '$' | '%' | '&' | '\'' | '*' | '+' | '-' | '.' | '^' | '_' |'`' | '|' | '~' | '0'..='9' | 'A'..='Z' | 'a'..='z');
game!(MESSAGE = ([(name @ [TCHAR; 1..], HEADER_FIELD_NAME_DELIMITER, (value @ [_; ..]), HEADER_FIELD_DELIMITER); 1..], "\r\n", content @ [_; ..]));

/// A message from the language server.
#[derive(Debug, ParseDisplay)]
pub(crate) enum ServerMessage {
    /// A response.
    #[display("Response: {0}")]
    Response(ServerResponse),
    /// A request.
    #[display("Request[{id:?}]: {request}")]
    Request {
        /// The id.
        id: Id,
        /// The request.
        request: ServerRequest,
    },
    /// A notification.
    #[display("Notification: {0}")]
    Notification(ServerNotification),
}

impl TryFrom<Message> for ServerMessage {
    type Error = UnknownServerMessageFailure;

    #[throws(Self::Error)]
    fn try_from(other: Message) -> Self {
        match other.content.into() {
            Kind::Request {
                id: Some(request_id),
                method,
                params,
            } => Self::Request {
                id: request_id,
                request: ServerRequest::new(method, params)?,
            },
            Kind::Request {
                id: None,
                method,
                params,
            } => Self::Notification(ServerNotification::new(method, params)?),
            Kind::Response {
                outcome: Outcome::Result(value),
                ..
            } => Self::Response(value.try_into()?),
        }
    }
}

/// A notification from the server.
#[derive(Debug, ParseDisplay)]
pub(crate) enum ServerNotification {
    /// Sends diagnostics to the client.
    #[display("PublishDiagnostics({0:?})")]
    PublishDiagnostics(PublishDiagnosticsParams),
}

impl ServerNotification {
    /// Creates a new `ServerNotification`.
    #[throws(UnknownServerMessageFailure)]
    fn new(method: String, params: Params) -> Self {
        match method.as_str() {
            <PublishDiagnostics as Notification>::METHOD => Self::PublishDiagnostics(
                serde_json::from_value::<PublishDiagnosticsParams>(params.clone().into()).map_err(
                    |error| UnknownServerMessageFailure::InvalidParams {
                        method,
                        params,
                        error,
                    },
                )?,
            ),
            _ => throw!(UnknownServerMessageFailure::UnknownMethod(method)),
        }
    }
}

/// A reqeust from the server.
#[derive(Debug, ParseDisplay)]
pub(crate) enum ServerRequest {
    /// Registers capabilities with the client.
    #[display("RegisterCapability({0:?})")]
    RegisterCapability(RegistrationParams),
}

impl ServerRequest {
    /// Creates a new `ServerRequest`.
    #[throws(UnknownServerMessageFailure)]
    fn new(method: String, params: Params) -> Self {
        match method.as_str() {
            <RegisterCapability as Request>::METHOD => Self::RegisterCapability(
                serde_json::from_value::<RegistrationParams>(params.clone().into()).map_err(
                    |error| UnknownServerMessageFailure::InvalidParams {
                        method,
                        params,
                        error,
                    },
                )?,
            ),
            _ => throw!(UnknownServerMessageFailure::UnknownMethod(method)),
        }
    }
}

/// A response from the server.
#[derive(Debug, ParseDisplay)]
pub(crate) enum ServerResponse {
    /// Response to an initialization request.
    #[display("{0:?}")]
    Initialize(InitializeResult),
    /// Response to a shutdown request.
    Shutdown,
}

impl TryFrom<Value> for ServerResponse {
    type Error = UnknownServerResponseFailure;

    #[inline]
    #[throws(Self::Error)]
    fn try_from(other: Value) -> Self {
        if let Ok(result) = serde_json::from_value::<InitializeResult>(other.clone()) {
            Self::Initialize(result)
        } else if serde_json::from_value::<()>(other).is_ok() {
            Self::Shutdown
        } else {
            throw!(UnknownServerResponseFailure);
        }
    }
}

/// Represents an LSP message.
// Do not use ParseDisplay as this requires Object be public.
#[derive(Debug)]
pub struct Message {
    /// The JSON-RPC object of the message.
    content: Object,
}

impl Message {
    /// Returns the header contained in `string`.
    fn header(string: &str) -> Option<&str> {
        string
            .find(HEADER_END)
            .and_then(|header_length| string.get(..header_length))
    }
}

impl AssembleFrom<u8> for Message {
    type Error = AssembleMessageError;

    #[inline]
    #[throws(AssembleFailure<AssembleMessageError>)]
    fn assemble_from(parts: &mut Vec<u8>) -> Self {
        // TODO: This would probably be simpler with a regex.
        let mut length = 0;

        let string = std::str::from_utf8(parts).map_err(Self::Error::from)?;
        let header = Self::header(string).ok_or(AssembleFailure::Incomplete)?;
        // saturating_add will not reach end due to full header_len needing to exist in string.
        let header_len = header.len().saturating_add(HEADER_END.len());
        let mut content_length: Option<usize> = None;

        for field in header.split(HEADER_FIELD_DELIMITER) {
            let mut items = field.split(": ");

            if items.next() == Some(HEADER_CONTENT_LENGTH) {
                if let Some(content_length_str) = items.next() {
                    if let Ok(value) = content_length_str.parse() {
                        content_length = Some(value);
                    }
                }

                break;
            }
        }

        // Cannot return from function until after parts.drain() is called.
        let object: Result<Object, _> = match content_length {
            None => {
                length = header_len;
                Err(AssembleFailure::Error(
                    AssembleMessageError::MissingContentLength,
                ))
            }
            Some(content_length) => {
                #[allow(clippy::option_if_let_else)]
                // False trigger. See https://github.com/rust-lang/rust-clippy/issues/5822.
                if let Some(total_len) = header_len.checked_add(content_length) {
                    if parts.len() < total_len {
                        Err(AssembleFailure::Incomplete)
                    } else if let Some(content) = string.get(header_len..total_len) {
                        length = total_len;
                        serde_json::from_str(content).map_err(|error| {
                            AssembleFailure::Error(AssembleMessageError::from(error))
                        })
                    } else {
                        length = header_len;
                        Err(AssembleFailure::Error(
                            AssembleMessageError::InvalidContentLength,
                        ))
                    }
                } else {
                    length = header_len;
                    Err(AssembleFailure::Error(
                        AssembleMessageError::InvalidContentLength,
                    ))
                }
            }
        };

        let _ = parts.drain(..length);
        object?.into()
    }
}

impl Display for Message {
    #[inline]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.content)
    }
}

impl From<Object> for Message {
    #[inline]
    fn from(value: Object) -> Self {
        Self { content: value }
    }
}

#[allow(clippy::use_self)] // False positive on format!.
impl DisassembleFrom<Message> for u8 {
    type Error = SerdeJsonError;

    #[inline]
    #[throws(Self::Error)]
    fn disassemble_from(good: Message) -> Vec<Self> {
        let content = serde_json::to_string(&good.content)?;

        format!(
            "{}{}{}{}{}",
            HEADER_CONTENT_LENGTH,
            HEADER_FIELD_NAME_DELIMITER,
            content.len(),
            HEADER_END,
            content
        )
        .as_bytes()
        .to_vec()
    }
}

/// Error while assembling a `Message`.
#[derive(Debug, ThisError)]
pub enum AssembleMessageError {
    /// Received bytes were not valid utf8.
    #[error(transparent)]
    Utf8(#[from] Utf8Error),
    /// Content length was not found in header.
    #[error("Header is missing content length")]
    MissingContentLength,
    /// Content length is invalid.
    #[error("content length is invalid")]
    InvalidContentLength,
    /// Unable to convert message.
    #[error("messge content is invalid: {0}")]
    InvalidContent(#[from] SerdeJsonError),
}

/// Response from server is unknown.
#[derive(Clone, Copy, Debug, ThisError)]
#[error("Unknown response from server")]
pub struct UnknownServerResponseFailure;

/// Message from server is unknown.
#[derive(Debug, ThisError)]
pub enum UnknownServerMessageFailure {
    /// Response is unknown.
    #[error(transparent)]
    Response(#[from] UnknownServerResponseFailure),
    /// Method from Server is unknown.
    #[error("Unknown method: {0}")]
    UnknownMethod(String),
    /// The `params` do not match with `method`.
    #[error("Unable to convert `{method}` from `{params}`: {error}")]
    InvalidParams {
        /// The method.
        method: String,
        /// The parameters.
        params: Params,
        /// The error.
        #[source]
        error: SerdeJsonError,
    },
}
