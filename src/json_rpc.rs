use {
    core::fmt::{self, Debug, Display},
    parse_display::Display as ParseDisplay,
    serde::{Deserialize, Serialize},
    serde_json::{Map, Number, Value},
};

pub(crate) trait Method {
    fn method(&self) -> String;
    fn params(&self) -> Params;
}

pub(crate) trait Success {
    fn result(&self) -> Value;
}

/// A JSON-RPC object.
#[derive(Clone, Debug, Deserialize, ParseDisplay, PartialEq, Serialize)]
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
    pub(crate) fn new(kind: Kind) -> Self {
        Self {
            jsonrpc: Version::V2_0,
            kind,
        }
    }

    /// Creates a Request object.
    pub(crate) fn request<M: Method>(id: Id, method: M) -> Self
    {
        Self::new(Kind::Request {
            method: method.method(),
            params: method.params(),
            id: Some(id),
        })
    }

    /// Creates a Notification object.
    pub(crate) fn notification<M: Method>(method: M) -> Self
    {
        Self::new(Kind::Request {
            method: method.method(),
            params: method.params(),
            id: None,
        })
    }

    /// Creates a Response object.
    pub(crate) fn response<S: Success>(id: Id, success: S) -> Self
    {
        Self::new(Kind::Response {
            id,
            outcome: Outcome::Result(success.result()),
        })
    }
}

/// A type of JSON-RPC object.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub(crate) enum Kind {
    /// A JSON-RPC Request object.
    Request {
        method: String,
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
        write!(f, "{}", match self {
            Self::Request { id: Some(id), method, params } => format!("Request[{}]: {} w/ {}", id, method, params),
            Self::Request { id: None, method , params} => format!("Notification: {} w/ {}", method, params),
            Self::Response { outcome, id } => format!("Response[{}]: {}", id, outcome),
        })
    }
}

impl From<Object> for Kind {
    fn from(value: Object) -> Self {
        value.kind
    }
}

/// The outcome contained in a JSON-RPC Response.
#[derive(Clone, Debug, Deserialize, ParseDisplay, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Outcome {
    #[display("Success {0}")]
    /// A successful outcome.
    Result(Value),
}

#[derive(Clone, Debug, Deserialize, ParseDisplay, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Params {
    None,
    #[display("{0:?}")]
    Array(Vec<Value>),
    #[display("{0:?}")]
    Object(Map<String, Value>),
}

impl From<Value> for Params {
    fn from(value: Value) -> Self {
        match value {
            Value::Null => Self::None,
            Value::Bool(_) | Value::Number(_) | Value::String(_) => Self::Array(vec![value]),
            Value::Array(seq) => Self::Array(seq),
            Value::Object(map) => Self::Object(map),
        }
    }
}

impl From<Params> for Value {
    fn from(value: Params) -> Self {
        match value {
            Params::None => Self::Null,
            Params::Array(v) => Self::Array(v),
            Params::Object(m) => Self::Object(m),
        }
    }
}

/// The identifier of a JSON-RPC Request.
#[derive(Clone, Debug, Deserialize, Eq, ParseDisplay, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[serde(untagged)]
pub enum Id {
    /// A null id.
    #[display("NULL")]
    Null,
    /// A numeric id.
    #[display("{0}")]
    Num(Number),
    /// A string id.
    #[display("{0}")]
    Str(String),
}

impl From<u64> for Id {
    fn from(value: u64) -> Self {
        Id::Num(value.into())
    }
}

/// The JSON-RPC protocol version.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum Version {
    /// Version 2.0.
    #[serde(rename = "2.0")]
    V2_0
}
