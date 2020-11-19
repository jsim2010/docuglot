//! Implements JSON-RPC functionality.
use core::fmt::{self, Debug, Display};

/// Represents a JSON-RPC method.
pub(crate) trait Method {
    /// Returns the method of `self`.
    fn method(&self) -> String;
    /// Returns the parameters of `self`.
    fn params(&self) -> Params;
}

/// Represents a successful method completion.
pub(crate) trait Success {
    /// Returns the result of `self`.
    fn result(&self) -> serde_json::Value;
}

/// A JSON-RPC object.
#[derive(Clone, Debug, serde::Deserialize, parse_display::Display, PartialEq, serde::Serialize)]
#[display("{kind}")]
pub(crate) struct Object {
    /// The JSON version.
    jsonrpc: Version,
    /// The kind of the JSON-RPC object.
    #[serde(flatten)]
    kind: Kind,
}

impl Object {
    /// Creates a new `Object`.
    pub(crate) const fn new(kind: Kind) -> Self {
        Self {
            jsonrpc: Version::V2_0,
            kind,
        }
    }

    /// Creates a Request object.
    pub(crate) fn request<M: Method>(id: Id, method: &M) -> Self {
        Self::new(Kind::Request {
            method: method.method(),
            params: method.params(),
            id: Some(id),
        })
    }

    /// Creates a Notification object.
    pub(crate) fn notification<M: Method>(method: &M) -> Self {
        Self::new(Kind::Request {
            method: method.method(),
            params: method.params(),
            id: None,
        })
    }

    /// Creates a Response object.
    pub(crate) fn response<S: Success>(id: Id, success: &S) -> Self {
        Self::new(Kind::Response {
            id,
            outcome: Outcome::Result(success.result()),
        })
    }
}

/// A type of JSON-RPC object.
#[derive(Clone, Debug, serde::Deserialize, PartialEq, serde::Serialize)]
#[serde(untagged)]
pub(crate) enum Kind {
    /// A JSON-RPC Request object.
    Request {
        /// The method.
        method: String,
        /// The parameters of `method.
        params: Params,
        /// The id.
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<Id>,
    },
    /// A JSON-RPC Response object.
    Response {
        /// The outcome.
        #[serde(flatten)]
        outcome: Outcome,
        /// The id.
        id: Id,
    },
}

impl Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Request {
                    id: Some(id),
                    method,
                    params,
                } => format!("Request[{}]: {} w/ {}", id, method, params),
                Self::Request {
                    id: None,
                    method,
                    params,
                } => format!("Notification: {} w/ {}", method, params),
                Self::Response { outcome, id } => format!("Response[{}]: {}", id, outcome),
            }
        )
    }
}

impl From<Object> for Kind {
    fn from(value: Object) -> Self {
        value.kind
    }
}

/// The outcome contained in a JSON-RPC Response.
#[derive(Clone, Debug, serde::Deserialize, parse_display::Display, PartialEq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Outcome {
    #[display("Success {0}")]
    /// A successful outcome.
    Result(serde_json::Value),
}

/// The parameters of a JSON-RPC method.
#[derive(Clone, Debug, serde::Deserialize, parse_display::Display, PartialEq, serde::Serialize)]
#[serde(untagged)]
pub enum Params {
    /// Represents no parameters.
    None,
    /// Represents an array of parameters.
    #[display("{0:?}")]
    Array(Vec<serde_json::Value>),
    /// Represents a map of parameters.
    #[display("{0:?}")]
    Object(serde_json::Map<String, serde_json::Value>),
}

impl From<serde_json::Value> for Params {
    fn from(value: serde_json::Value) -> Self {
        match value {
            serde_json::Value::Null => Self::None,
            serde_json::Value::Bool(_)
            | serde_json::Value::Number(_)
            | serde_json::Value::String(_) => Self::Array(vec![value]),
            serde_json::Value::Array(seq) => Self::Array(seq),
            serde_json::Value::Object(map) => Self::Object(map),
        }
    }
}

impl From<Params> for serde_json::Value {
    #[inline]
    fn from(value: Params) -> Self {
        match value {
            Params::None => Self::Null,
            Params::Array(v) => Self::Array(v),
            Params::Object(m) => Self::Object(m),
        }
    }
}

/// The identifier of a JSON-RPC Request.
#[derive(
    Clone, Debug, serde::Deserialize, Eq, parse_display::Display, PartialEq, serde::Serialize,
)]
#[serde(deny_unknown_fields)]
#[serde(untagged)]
pub enum Id {
    /// A null id.
    #[display("NULL")]
    Null,
    /// A numeric id.
    #[display("{0}")]
    Num(serde_json::Number),
    /// A string id.
    #[display("{0}")]
    Str(String),
}

impl From<u64> for Id {
    fn from(value: u64) -> Self {
        Self::Num(value.into())
    }
}

/// The JSON-RPC protocol version.
#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, serde::Serialize)]
enum Version {
    /// Version 2.0.
    #[serde(rename = "2.0")]
    V2_0,
}
