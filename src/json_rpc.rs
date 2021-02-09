//! Defines JSON-RPC structures and processing.
use {
    core::fmt::{self, Formatter, Debug, Display},
    fehler::{throw, throws},
    serde::{Deserialize, Serialize},
    serde_json::{Map, Number, Value},
    std::{
        collections::hash_map::{Entry, HashMap},
        sync::atomic::{AtomicU64, Ordering},
    },
};

/// The minimum value of a predefined error code.
#[allow(clippy::decimal_literal_representation)] // Decimal version is used on specification.
const PREDEFINED_ERROR_MIN_CODE: i64 = -32768;
/// A code value indicating a parse error.
const PARSE_ERROR_CODE: i64 = -32700;
/// A code value indicating an invalid request error.
const INVALID_REQUEST_CODE: i64 = -32600;
/// A code value indicating a method not found error.
const METHOD_NOT_FOUND_CODE: i64 = -32601;
/// A code value indicating an invalid params error.
const INVALID_PARAMS_CODE: i64 = -32602;
/// A code value indicating an internal error.
const INTERNAL_ERROR_CODE: i64 = -32603;
/// The minimum value of a server error code.
const SERVER_ERROR_MIN_CODE: i64 = -32099;
/// The maximum value of a predefined error code.
const PREDEFINED_ERROR_MAX_CODE: i64 = -32000;
/// The maximum value of a server error code.
const SERVER_ERROR_MAX_CODE: i64 = PREDEFINED_ERROR_MAX_CODE;

/// The message of a method not found error.
const METHOD_NOT_FOUND_MESSAGE: &str = "Method not found";

/// The handler that defines how a client processes a successful response.
pub(crate) type ResultHandler<S, O> = fn(&mut S, Value) -> Option<Result<O, serde_json::Error>>;

/// The handler that defines how a client processes an error response.
pub(crate) type ErrorHandler<S, O> = fn(&mut S, ErrorObject) -> Option<O>;

/// The handler that defines how a server processes a request that expects a response.
pub(crate) type RequestHandler<S> = fn(&mut S, Params) -> Outcome;

/// The handler that defines how a server processes a notification.
pub(crate) type NotificationHandler<S> = fn(&mut S, Params);

/// The handlers that define how a client processes a response.
///
/// `O` is the type of the output generated after processing completes.
pub(crate) type ResponseHandlers<S, O> = (ResultHandler<S, O>, ErrorHandler<S, O>);

/// The handlers that define how a client processes a request.
pub(crate) type MethodHandlers<S> = (Option<RequestHandler<S>>, Option<NotificationHandler<S>>);

/// Defines the characteristics of a JSON-RPC request.
pub(crate) trait Request<S, O> {
    /// The name of the method being requested.
    const METHOD: &'static str;

    /// Returns the [`Params`] of the method.
    ///
    /// # Error(s)
    ///
    /// Throws [`serde_json::Error`] if unable to serialize the parameters of the method.
    #[throws(serde_json::Error)]
    fn params(&self) -> Params;

    /// Returns the calls that handle a success or error response to this request.
    ///
    /// [`None`] indicates the request is a notification.
    fn response_handlers(&self) -> Option<ResponseHandlers<S, O>> {
        None
    }

    /// Returns the [`MethodHandlers`] appropriate for the request.
    fn method_handlers(&self) -> MethodHandlers<S> {
        (None, None)
    }
}

/// A JSON-RPC object.
#[derive(Clone, Debug, Deserialize, parse_display::Display, PartialEq, Serialize)]
#[display("{kind}")]
pub(crate) struct Object {
    /// The version of the JSON-RPC protocol.
    jsonrpc: Version,
    /// The kind of the JSON-RPC object.
    #[serde(flatten)]
    kind: Kind,
}

impl From<Kind> for Object {
    fn from(kind: Kind) -> Self {
        Self {
            jsonrpc: Version::V2_0,
            kind,
        }
    }
}

impl From<RequestObject> for Object {
    fn from(request: RequestObject) -> Self {
        Self::from(Kind::from(request))
    }
}

impl From<ResponseObject> for Object {
    fn from(response: ResponseObject) -> Self {
        Self::from(Kind::from(response))
    }
}

/// The JSON-RPC protocol version.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum Version {
    /// Version 2.0.
    ///
    /// MUST be exactly "2.0".
    #[serde(rename = "2.0")]
    V2_0,
}

/// A type of JSON-RPC object.
#[derive(Clone, Debug, Deserialize, parse_display::Display, PartialEq, Serialize)]
#[serde(untagged)]
pub(crate) enum Kind {
    /// A JSON-RPC Request object.
    #[display("{0}")]
    Request(RequestObject),
    /// A JSON-RPC Response object.
    #[display("{0}")]
    Response(ResponseObject),
}

impl From<RequestObject> for Kind {
    fn from(request: RequestObject) -> Self {
        Self::Request(request)
    }
}

impl From<ResponseObject> for Kind {
    fn from(response: ResponseObject) -> Self {
        Self::Response(response)
    }
}

impl From<Object> for Kind {
    fn from(value: Object) -> Self {
        value.kind
    }
}

/// An object sent by a client to notify a server of a request.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct RequestObject {
    /// The name of the method to be invoked.
    method: String,
    /// The parameter values to be used during the invocation of the method.
    params: Params,
    /// An identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Id>,
}

impl Display for RequestObject {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match &self.id {
                Some(id) => format!("Request[{}]: '{}' w/ {}", id, self.method, self.params),
                None => format!("Notification: '{}' w/ {}", self.method, self.params),
            }
        )
    }
}

/// An Object or Array value that holds the parameters of a JSON-RPC method.
#[derive(Clone, Debug, Deserialize, parse_display::Display, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Params {
    /// No parameters.
    None,
    /// An array of parameters.
    #[display("{0:?}")]
    Array(Vec<Value>),
    /// A map of parameters.
    #[display("{0:?}")]
    Object(Map<String, Value>),
}

impl From<Value> for Params {
    fn from(value: Value) -> Self {
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

impl From<Params> for Value {
    #[inline]
    fn from(params: Params) -> Self {
        match params {
            Params::None => Self::Null,
            Params::Array(array) => Self::Array(array),
            Params::Object(map) => Self::Object(map),
        }
    }
}

/// The identifier of a JSON-RPC Request.
///
/// MUST contain a String, Number, or NULL value.
#[derive(Clone, Debug, Deserialize, Eq, parse_display::Display, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[serde(untagged)]
pub(crate)enum Id {
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
    fn from(num_id: u64) -> Self {
        Self::Num(num_id.into())
    }
}

/// An object returned by a server after processing a request.
#[derive(Clone, Debug, Deserialize, parse_display::Display, PartialEq, Serialize)]
#[display("Response[{id}]: {outcome}")]
pub(crate) struct ResponseObject {
    /// The outcome.
    #[serde(flatten)]
    outcome: Outcome,
    /// The id.
    id: Id,
}

/// The outcome contained in a JSON-RPC Response.
#[derive(Clone, Debug, Deserialize, parse_display::Display, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Outcome {
    #[display("Success {0}")]
    /// A successful outcome.
    Result(Value),
    #[display("{0}")]
    /// An error.
    Error(ErrorObject),
}

impl Outcome {
    /// Returns an [`Outcome`] that is a method not found error.
    pub(crate) fn unknown_request() -> Self {
        Self::Error(ErrorObject {
            code: METHOD_NOT_FOUND_CODE,
            message: METHOD_NOT_FOUND_MESSAGE.to_string(),
            data: None,
        })
    }

    /// Returns an invalid state error.
    pub(crate) fn invalid_state() -> Self {
        Self::Error(ErrorObject {
            code: -127,
            message: "Invalid state".to_string(),
            data: None,
        })
    }

    /// Returns an invalid params error.
    pub(crate) fn invalid_params(error: &serde_json::Error) -> Self {
        Self::Error(ErrorObject {
            code: INVALID_PARAMS_CODE,
            message: "Invalid method parameter(s)".to_string(),
            data: Some(Value::String(error.to_string())),
        })
    }
}

/// An object returned by a server to indicate an error was encountered.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, thiserror::Error)]
#[error("Error[{code}]: {message}")]
pub struct ErrorObject {
    /// Indicates the error type that occurred.
    code: i64,
    /// Provides a short description of the error.
    message: String,
    /// Contains additional information about the error.
    data: Option<Value>,
}

/// Implements the processing performed by a JSON-RPC client.
///
/// `O` is the type of the output generated after processing a response.
pub(crate) struct Client<S, O> {
    /// The value of the [`Id`] of the next request sent by [`Client`].
    ///
    /// The [`Id`]s of [`Client`] are implemented as sequential numbers.
    next_id: AtomicU64,
    /// Stores the handlers associated with all requests for which [`Client`] has not received a response.
    response_handlers: HashMap<u64, ResponseHandlers<S, O>>,
}

impl<S, O> Client<S, O> {
    /// Creates a new [`Client`].
    pub(crate) fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            response_handlers: HashMap::new(),
        }
    }

    /// Creates a [`RequestObject`] from `request` and stores its [`ResponseHandlers`].
    ///
    /// # Error(s)
    ///
    /// Throws [`serde_json::Error`] if unable to serialize the parameters of `request`.
    #[throws(serde_json::Error)]
    pub(crate) fn request<R: Request<S, O>>(&mut self, request: &R) -> RequestObject {
        let request = RequestObject {
            method: R::METHOD.to_string(),
            params: request.params()?,
            id: request.response_handlers().map(|handlers| {
                // Increment self.next_id until we find an id that is not currently awaiting a response.
                // Theoretically, it is possible this could turn into an infinite loop if all possible 2^64 ids were used. Pratically, this is unreasonable.
                loop {
                    let id_num = self.next_id.fetch_add(1, Ordering::Relaxed);

                    if let Entry::Vacant(entry) = self.response_handlers.entry(id_num) {
                        #[allow(unused_results)] // Reference to inserted value is unneeded.
                        {
                            entry.insert(handlers);
                        }

                        break Id::Num(id_num.into());
                    }
                }
            }),
        };
        log::trace!("{}", request);
        request
    }

    /// Returns the call and parameter for handling `response`.
    #[throws(ProcessResponseError)]
    pub(crate) fn process_response(
        &mut self,
        state: &mut S,
        response: ResponseObject,
    ) -> Option<O> {
        let mut response_handlers = None;
        if let Id::Num(ref id) = response.id {
            if let Some(id_u64) = id.as_u64() {
                response_handlers = self.response_handlers.remove(&id_u64);
            }
        }

        if let Some(handlers) = response_handlers {
            match response.outcome {
                Outcome::Result(value) => (handlers.0)(state, value).transpose()?,
                Outcome::Error(ref error_object) => {
                    match Error::from(error_object.clone()) {
                        Error::Application(error) => (handlers.1)(state, error),
                        Error::Predefined(error) => throw!(error),
                    }
                }
            }
        } else {
            log::warn!("Received response with unrecognized id '{}'", response.id);
            None
        }
    }
}

/// An error was cuaght during response processing.
#[derive(Debug, thiserror::Error)]
pub enum ProcessResponseError {
    /// Predefined.
    #[error(transparent)]
    Predefined(#[from] PredefinedError),
    /// Serialize.
    #[error(transparent)]
    Serialize(#[from] serde_json::Error),
}

/// An error was caught when inserting a request into the server list.
#[derive(Debug, thiserror::Error)]
pub struct InsertRequestError {
    /// The method on which the error occurred.
    method: String,
    /// The error occurred on a request - otherwise it occurred on a notification.
    was_request: bool,
}

impl Display for InsertRequestError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{} handler for `{}` already existed", if self.was_request { "request" } else { "notification"}, self.method)
    }
}

/// Inserts `request` into the appropriate handler map.
#[throws(InsertRequestError)]
fn insert_request<S, O, R: Request<S, O>>(
    request_handlers: &mut HashMap<&'static str, RequestHandler<S>>,
    notification_handlers: &mut HashMap<&'static str, NotificationHandler<S>>,
    request: &R,
) {
    let (req, notification) = request.method_handlers();

    if let Some(request_handler) = req {
        if request_handlers.insert(R::METHOD, request_handler).is_some() {
            throw!(InsertRequestError {
                method: R::METHOD.to_string(),
                was_request: true,
            });
        }
    }

    if let Some(notification_handler) = notification {
        if notification_handlers.insert(R::METHOD, notification_handler).is_some() {
            throw!(InsertRequestError {
                method: R::METHOD.to_string(),
                was_request: false,
            });
        }
    }
}

/// Implements the processing performed by a JSON-RPC server.
///
/// A processor of `P` processes received requests and returns an output of type `O`.
pub(crate) struct Server<S> {
    /// Associates [`RequestHandler`]s with their method.
    request_handlers: HashMap<&'static str, RequestHandler<S>>,
    /// Associates [`NotificationHandler`]s with their method.
    notification_handlers: HashMap<&'static str, NotificationHandler<S>>,
}

impl<S> Server<S> {
    /// Creates a new [`Server`] that processes `requests`.
    #[throws(InsertRequestError)]
    pub(crate) fn new<O, R: Request<S, O>>(requests: Vec<R>) -> Self {
        let mut request_handlers = HashMap::new();
        let mut notification_handlers = HashMap::new();

        for request in requests {
            insert_request(&mut request_handlers, &mut notification_handlers, &request)?;
        }

        Self {
            request_handlers,
            notification_handlers,
        }
    }

    /// Calls the appropriate handler for `request`, returning an output of type `O` and [`ResponseObject`] when appropriate.
    pub(crate) fn process_request(
        &mut self,
        state: &mut S,
        request: RequestObject,
    ) -> Option<ResponseObject> {
        if let Some(id) = request.id {
            let outcome =
                if let Some(request_handler) = self.request_handlers.get(request.method.as_str()) {
                    (request_handler)(state, request.params)
                } else {
                    log::warn!("Received request with unknown method: {}", request.method);
                    Outcome::unknown_request()
                };

            Some(ResponseObject { id, outcome })
        } else if let Some(notification_handler) =
            self.notification_handlers.get(request.method.as_str())
        {
            (notification_handler)(state, request.params);
            None
        } else {
            log::warn!(
                "Received notification with unknown method: {}",
                request.method
            );
            None
        }
    }
}

/// An error returned from JSON-RPC.
#[derive(Debug, thiserror::Error, market::ConsumeFault)]
pub(crate) enum Error {
    /// An error predefined by the JSON-RPC specification.
    #[error(transparent)]
    Predefined(PredefinedError),
    /// An error defined by the application.
    #[error(transparent)]
    Application(ErrorObject),
}

impl From<ErrorObject> for Error {
    fn from(error: ErrorObject) -> Self {
        match error.code {
            PREDEFINED_ERROR_MIN_CODE..=PREDEFINED_ERROR_MAX_CODE => match error.code {
                PARSE_ERROR_CODE => Self::Predefined(PredefinedError::Parse(error.data)),
                INVALID_REQUEST_CODE => {
                    Self::Predefined(PredefinedError::InvalidRequest(error.data))
                }
                METHOD_NOT_FOUND_CODE => {
                    Self::Predefined(PredefinedError::MethodNotFound(error.data))
                }
                INVALID_PARAMS_CODE => Self::Predefined(PredefinedError::InvalidParams(error.data)),
                INTERNAL_ERROR_CODE => Self::Predefined(PredefinedError::Internal(error.data)),
                SERVER_ERROR_MIN_CODE..=SERVER_ERROR_MAX_CODE => {
                    Self::Predefined(PredefinedError::Server(error))
                }
                _ => Self::Predefined(PredefinedError::Reserved(error)),
            },
            _ => Self::Application(error),
        }
    }
}

/// An error predefined by the JSON-RPC specification.
#[derive(Debug, thiserror::Error, market::ConsumeFault)]
pub enum PredefinedError {
    /// A parse error.
    #[error("An error occurred while parsing the JSON text")]
    Parse(Option<Value>),
    /// An invalid request error.
    #[error("JSON is not a valid Request object")]
    InvalidRequest(Option<Value>),
    /// A method not found error.
    #[error("The method does not exist/is not available")]
    MethodNotFound(Option<Value>),
    /// An invalid params error.
    #[error("Invalid method parameter(s)")]
    InvalidParams(Option<Value>),
    /// An internal error.
    #[error("Internal JSON-RPC error")]
    Internal(Option<Value>),
    /// A server error.
    #[error("Implementation-defined server error")]
    Server(ErrorObject),
    /// An error using a reserved code.
    #[error("")]
    Reserved(ErrorObject),
}
