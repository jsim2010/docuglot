//! Implements the language server protocol functionality of docuglot.
#![allow(clippy::wildcard_imports)] // Need to mark cur so that wildcard import is okay
use {
    crate::json_rpc::{
        Client, InsertRequestError, Kind, MethodHandlers, Object, Outcome, Params,
        ProcessResponseError, Request, RequestObject, ResponseHandlers, ResponseObject, Server,
    },
    core::{
        fmt::{self, Display, Formatter},
        str::Utf8Error,
        task::Poll,
    },
    fehler::{throw, throws},
    lsp_types::{
        ClientCapabilities, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
        DocumentSymbolClientCapabilities, DocumentSymbolParams, DocumentSymbolResponse,
        InitializeParams, InitializeResult, InitializedParams, RegistrationParams,
        TextDocumentClientCapabilities, TextDocumentSyncClientCapabilities, Url,
    },
    market::{Consumer, Flawless, Flaws, Producer, ProductionFlaws, Recall},
    market_types::{
        compose::Composite,
        io::{WriteDefect, Writer},
        process::{NoWaitChildStdin, Product, ProductConsumer},
    },
    serde::{Deserialize, Serialize},
    serde_json::Value,
    std::process::{self, Command, ExitStatus},
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

/// Returns an initialize params for the tool.
fn initialize_params(root_dir: &Url) -> InitializeParams {
    #[allow(deprecated)] // root_path is required by InitializeParams.
    InitializeParams {
        process_id: Some(process::id()),
        root_path: None,
        root_uri: Some(root_dir.clone()),
        initialization_options: None,
        capabilities: ClientCapabilities {
            workspace: None,
            text_document: Some(TextDocumentClientCapabilities {
                synchronization: Some(TextDocumentSyncClientCapabilities {
                    dynamic_registration: None,
                    will_save: None,
                    will_save_wait_until: None,
                    did_save: None,
                }),
                completion: None,
                hover: None,
                signature_help: None,
                references: None,
                document_highlight: None,
                document_symbol: Some(DocumentSymbolClientCapabilities {
                    dynamic_registration: None,
                    symbol_kind: None,
                    hierarchical_document_symbol_support: Some(true),
                    tag_support: None,
                }),
                formatting: None,
                range_formatting: None,
                on_type_formatting: None,
                declaration: None,
                definition: None,
                type_definition: None,
                implementation: None,
                code_action: None,
                code_lens: None,
                document_link: None,
                color_provider: None,
                rename: None,
                publish_diagnostics: None,
                folding_range: None,
                selection_range: None,
                linked_editing_range: None,
                call_hierarchy: None,
                semantic_tokens: None,
                moniker: None,
            }),
            window: None,
            general: None,
            experimental: None,
        },
        trace: None,
        workspace_folders: None,
        client_info: None,
        locale: None,
    }
}

/// A message to the language server.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Directive {
    /// The client received the initialization.
    Initialized,
    /// The client is shutting down.
    Shutdown,
    /// The client is exiting.
    Exit,
    /// The client requests a document symbol.
    DocumentSymbol(DocumentSymbolParams),
    /// The client opened a document.
    OpenDoc(DidOpenTextDocumentParams),
    /// The client closed a document.
    CloseDoc(DidCloseTextDocumentParams),
}

impl Display for Directive {
    #[allow(clippy::use_debug)] // Params do not impl Display.
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Initialized => write!(f, "Initialized"),
            Self::Shutdown => write!(f, "Shutdown"),
            Self::Exit => write!(f, "Exit"),
            Self::DocumentSymbol(ref params) => write!(f, "DocumentSymbol {0:?}", params),
            Self::OpenDoc(ref params) => write!(f, "OpenDoc {0:?}", params),
            Self::CloseDoc(ref params) => write!(f, "CloseDoc {0:?}", params),
        }
    }
}

/// An error when the [`Tool`] is in an invalid state.
#[derive(Debug, thiserror::Error)]
#[error("Tool is in invalid state: {state}")]
pub struct InvalidStateError {
    /// The [`State`] of the [`Tool`].
    state: State,
}

/// Describes events that the client must process.
#[derive(Debug, parse_display::Display)]
pub(crate) enum Event {
    /// The tool wants to send a message to the server.
    #[display("send message")]
    SendDirectives(Vec<Directive>),
    /// The tool encountered an error.
    #[display("error")]
    Error(InvalidStateError),
    /// The tool received a document symbol.
    #[display("")]
    DocumentSymbol(DocumentSymbolResponse),
    /// The tool has exited.
    #[display("")]
    Exit(ExitStatus),
}

impl From<ExitStatus> for Event {
    fn from(status: ExitStatus) -> Self {
        Self::Exit(status)
    }
}

/// An error when producing a transmission.
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    /// An error serializing the message to be transmitted.
    #[error(transparent)]
    Serialize(#[from] serde_json::Error),
    /// An error producing the message.
    #[error(transparent)]
    Recall(#[from] Recall<ProductionFlaws<WriteDefect>, u8>),
}

/// An error during transmission to the language server.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TransmissionError {
    /// An error during transmission.
    #[error(transparent)]
    Tx(#[from] WriteError),
    /// The tool is in an invalid state.
    #[error(transparent)]
    InvalidState(#[from] InvalidStateError),
}

impl From<serde_json::Error> for TransmissionError {
    fn from(ser_error: serde_json::Error) -> Self {
        Self::Tx(ser_error.into())
    }
}

impl From<Recall<ProductionFlaws<WriteDefect>, u8>> for TransmissionError {
    fn from(recall: Recall<ProductionFlaws<WriteDefect>, u8>) -> Self {
        Self::Tx(recall.into())
    }
}

/// An error during the reception of a message from the language server.
#[derive(Debug, thiserror::Error)]
pub enum ReceptionError {
    /// An error while producing a response.
    #[error(transparent)]
    Response(#[from] WriteError),
    /// A JSON-RPC error.
    #[error(transparent)]
    ProcessResponse(#[from] ProcessResponseError),
}

/// An error while composing an error message.
#[derive(Clone, Copy, Debug, thiserror::Error)]
#[error("Error while composing error message")]
pub(crate) struct ErrorMessageCompositionError;

/// An error message.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct ErrorMessage {
    /// The message.
    line: String,
}

impl Composite<u8> for ErrorMessage {
    type Misstep = ErrorMessageCompositionError;

    #[throws(Self::Misstep)]
    fn compose(elements: &mut Vec<u8>) -> Poll<Self> {
        if let Ok(s) = std::str::from_utf8_mut(elements) {
            #[allow(clippy::option_if_let_else)]
            // False trigger. See https://github.com/rust-lang/rust-clippy/issues/5822.
            if let Some(index) = s.find('\n') {
                let (l, remainder) = s.split_at_mut(index);
                let (_, new_elements) = remainder.split_at_mut(1);
                let line = (*l).to_string();
                *elements = new_elements.as_bytes().to_vec();

                Poll::Ready(Self { line })
            } else {
                // elements does not contain a new line.
                Poll::Pending
            }
        } else {
            // elements has some invalid uft8.
            *elements = Vec::new();
            throw!(ErrorMessageCompositionError);
        }
    }
}

impl Display for ErrorMessage {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.line)
    }
}

impl From<ErrorMessage> for Product<Message, ErrorMessage> {
    fn from(error_message: ErrorMessage) -> Self {
        Self::Error(error_message)
    }
}

/// Describes the state of the client.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum State {
    /// Server has not been initialized.
    Uninitialized {
        /// The desired root directory of the server.
        root_dir: Url,
    },
    /// Waiting for the server to confirm initialization.
    WaitingInitialization {
        /// [`Directive`]s to be sent after initialization is confirmed.
        directives: Vec<Directive>,
    },
    /// Normal running state.
    Running {
        /// The state of the server.
        server_state: Box<InitializeResult>,
        /// The registrations.
        registrations: Vec<lsp_types::Registration>,
    },
    /// Waiting for the server to confirm shutdown.
    WaitingShutdown,
    /// Waiting for the server to exit.
    WaitingExit,
}

impl Display for State {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Uninitialized { .. } => write!(f, "uninitialized"),
            Self::WaitingInitialization { .. } => write!(f, "waiting initialization"),
            Self::Running { .. } => write!(f, "running"),
            Self::WaitingShutdown => write!(f, "waiting shutdown"),
            Self::WaitingExit => write!(f, "waiting exit"),
        }
    }
}

/// The LSP client.
pub(crate) struct Tool {
    /// The writer
    lsp_writer: Writer<NoWaitChildStdin>,
    /// Consumes [`Product`] from the language server.
    lsp_consumer: ProductConsumer<Message, ErrorMessage>,
    /// The JSON-RPC client.
    rpc_client: Client<State, Event>,
    /// The JSON-RPC server.
    rpc_server: Server<State>,
    /// Defines the current state of `Self`.
    state: State,
}

impl Tool {
    /// Creates a new `Tool`.
    #[throws(CreateToolError)]
    pub(crate) fn new(command: Command, root_dir: Url) -> Self {
        let (lsp_writer, lsp_consumer) =
            market_types::process::spawn::<Message, ErrorMessage, _>(command, "Tool process")?;

        Self {
            lsp_writer,
            lsp_consumer,
            state: State::Uninitialized { root_dir },
            rpc_client: Client::new(),
            rpc_server: Server::new(vec![RegistrationParams {
                registrations: vec![],
            }])?,
        }
    }

    /// Creates a new [`Tool`] that is already waiting to exit.
    #[throws(CreateToolError)]
    pub(crate) fn new_finished(command: Command) -> Self {
        let (lsp_writer, lsp_consumer) = market_types::process::spawn(command, "Finished tool")?;

        Self {
            lsp_writer,
            lsp_consumer,
            state: State::WaitingExit,
            rpc_client: Client::new(),
            rpc_server: Server::new(vec![RegistrationParams {
                registrations: vec![],
            }])?,
        }
    }

    /// Writes `msg` to the language server.
    #[throws(WriteError)]
    fn write<M: Into<Message>>(&self, msg: M) {
        self.lsp_writer
            .produce_all(&mut msg.into().into_bytes()?.into_iter())?;
    }

    /// Transmits `directives` to the LSP server.
    #[throws(TransmissionError)]
    pub(crate) fn transmit(&mut self, mut directives: Vec<Directive>) {
        let mut request_objects = Vec::new();

        match self.state {
            State::Uninitialized { ref root_dir } => {
                request_objects.push(self.rpc_client.request(&initialize_params(root_dir))?);
                self.state = State::WaitingInitialization { directives };
            }
            State::WaitingInitialization {
                directives: ref mut pending_transmissions,
            } => {
                pending_transmissions.append(&mut directives);
            }
            State::Running { .. } => {
                for directive in directives {
                    request_objects.push(match directive {
                        Directive::Initialized => self.rpc_client.request(&InitializedParams {})?,
                        Directive::Shutdown => {
                            self.state = State::WaitingShutdown;
                            self.rpc_client.request(&ShutdownParams)?
                        }
                        Directive::Exit => self.rpc_client.request(&ExitParams {})?,
                        Directive::DocumentSymbol(params) => self.rpc_client.request(&params)?,
                        Directive::OpenDoc(params) => self.rpc_client.request(&params)?,
                        Directive::CloseDoc(params) => self.rpc_client.request(&params)?,
                    });
                }
            }
            State::WaitingShutdown | State::WaitingExit => {
                throw!(InvalidStateError {
                    state: self.state.clone()
                });
            }
        }

        for request_object in request_objects {
            self.write(request_object)?;
        }
    }

    /// Processes all receptions from LSP server.
    #[throws(ReceptionError)]
    pub(crate) fn process_receptions(&mut self) -> Vec<Event> {
        let mut receptions = Vec::new();

        match self.lsp_consumer.consume() {
            Ok(product) => match product {
                Product::Output(message) => {
                    let reception = match message.into() {
                        Kind::Request(request_object) => {
                            if let Some(response) = self
                                .rpc_server
                                .process_request(&mut self.state, request_object)
                            {
                                self.write(response)?;
                            }

                            None
                        }
                        Kind::Response(response) => self
                            .rpc_client
                            .process_response(&mut self.state, response)?,
                    };

                    if let Some(r) = reception {
                        receptions.push(r);
                    }
                }
                Product::Error(error_message) => {
                    log::error!("lsp stderr: {}", error_message);
                }
                Product::Exit(status) => {
                    receptions.push(status.into());
                }
                _ => {
                    log::error!("lsp received unknown product: {:?}", product);
                }
            },
            Err(failure) => {
                if failure.is_defect() {
                    log::error!("process receptions: {}", failure);
                }
            }
        }

        receptions
    }

    /// If state of `self` is waiting exit.
    pub(crate) fn is_waiting_exit(&self) -> bool {
        self.state == State::WaitingExit
    }
}

impl Request<State, Event> for InitializeParams {
    const METHOD: &'static str = "initialize";

    #[throws(serde_json::Error)]
    fn params(&self) -> Params {
        Params::from(serde_json::to_value(self)?)
    }

    fn response_handlers(&self) -> Option<ResponseHandlers<State, Event>> {
        Some((
            |state, value| {
                Some(match *state {
                    State::Uninitialized { .. }
                    | State::Running { .. }
                    | State::WaitingShutdown
                    | State::WaitingExit => Ok(Event::Error(InvalidStateError {
                        state: state.clone(),
                    })),
                    State::WaitingInitialization { ref mut directives } => {
                        match serde_json::from_value::<InitializeResult>(value) {
                            Ok(initialize_result) => {
                                let mut new_directives = vec![Directive::Initialized];
                                new_directives.append(directives);

                                *state = State::Running {
                                    server_state: Box::new(initialize_result),
                                    registrations: Vec::new(),
                                };

                                Ok(Event::SendDirectives(new_directives))
                            }
                            Err(error) => Err(error),
                        }
                    }
                })
            },
            |_, _| None,
        ))
    }
}

impl Request<State, Event> for DocumentSymbolParams {
    const METHOD: &'static str = "textDocument/documentSymbol";

    #[throws(serde_json::Error)]
    fn params(&self) -> Params {
        Params::from(serde_json::to_value(self)?)
    }

    fn response_handlers(&self) -> Option<ResponseHandlers<State, Event>> {
        Some((
            |_, value| Some(serde_json::from_value(value).map(Event::DocumentSymbol)),
            |_, _| None,
        ))
    }
}

/// Params of "shutdown" method.
struct ShutdownParams;

impl Request<State, Event> for ShutdownParams {
    const METHOD: &'static str = "shutdown";

    #[throws(serde_json::Error)]
    fn params(&self) -> Params {
        Params::from(serde_json::to_value(())?)
    }

    fn response_handlers(&self) -> Option<ResponseHandlers<State, Event>> {
        Some((
            |state, _| {
                Some(match *state {
                    State::Uninitialized { .. }
                    | State::Running { .. }
                    | State::WaitingInitialization { .. }
                    | State::WaitingExit => Ok(Event::Error(InvalidStateError {
                        state: state.clone(),
                    })),
                    State::WaitingShutdown => {
                        *state = State::WaitingExit;
                        Ok(Event::SendDirectives(vec![Directive::Exit]))
                    }
                })
            },
            |_, _| None,
        ))
    }
}

impl Request<State, Event> for InitializedParams {
    const METHOD: &'static str = "initialized";

    #[throws(serde_json::Error)]
    fn params(&self) -> Params {
        Params::from(serde_json::to_value(())?)
    }
}

impl Request<State, Event> for DidOpenTextDocumentParams {
    const METHOD: &'static str = "textDocument/didOpen";

    #[throws(serde_json::Error)]
    fn params(&self) -> Params {
        Params::from(serde_json::to_value(self)?)
    }
}

impl Request<State, Event> for DidCloseTextDocumentParams {
    const METHOD: &'static str = "textDocument/didClose";

    #[throws(serde_json::Error)]
    fn params(&self) -> Params {
        Params::from(serde_json::to_value(self)?)
    }
}

/// Params of "exit" method.
struct ExitParams;

impl Request<State, Event> for ExitParams {
    const METHOD: &'static str = "exit";

    #[throws(serde_json::Error)]
    fn params(&self) -> Params {
        Params::from(serde_json::to_value(())?)
    }
}

impl Request<State, Event> for RegistrationParams {
    const METHOD: &'static str = "client/registerCapability";

    #[throws(serde_json::Error)]
    fn params(&self) -> Params {
        Params::from(serde_json::to_value(self)?)
    }

    fn response_handlers(&self) -> Option<ResponseHandlers<State, Event>> {
        Some((|_, _| None, |_, _| None))
    }

    fn method_handlers(&self) -> MethodHandlers<State> {
        (
            Some(|state, params| match *state {
                State::Running {
                    ref mut registrations,
                    ..
                } => match serde_json::from_value::<Self>(params.into()) {
                    Ok(mut register) => {
                        registrations.append(&mut register.registrations);
                        Outcome::Result(Value::Null)
                    }
                    Err(error) => Outcome::invalid_params(&error),
                },
                State::Uninitialized { .. }
                | State::WaitingInitialization { .. }
                | State::WaitingExit
                | State::WaitingShutdown => Outcome::invalid_state(),
            }),
            None,
        )
    }
}

/// Represents an LSP message.
// Do not use parse_display::Display as this requires Object be public.
#[derive(Debug, Deserialize, Serialize)]
pub struct Message {
    /// The JSON-RPC object of the message.
    content: Object,
}

impl Message {
    /// Returns the header contained in `string`.
    #[throws(ComposeMessageMisstep)]
    fn header(string: &str) -> Option<&str> {
        if let Some(header_length) = string.find(HEADER_END) {
            Some(
                string
                    .get(..header_length)
                    .ok_or(ComposeMessageMisstep::InvalidHeaderEnd)?,
            )
        } else {
            None
        }
    }

    /// Converts `self` into a [`Vec<u8>`].
    #[throws(serde_json::Error)]
    fn into_bytes(self) -> Vec<u8> {
        let content = serde_json::to_string(&self.content)?;

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

impl Composite<u8> for Message {
    type Misstep = ComposeMessageMisstep;

    #[throws(Self::Misstep)]
    fn compose(elements: &mut Vec<u8>) -> Poll<Self> {
        let mut length = 0;

        let string = std::str::from_utf8(elements).map_err(Self::Misstep::from)?;
        if let Some(header) = Self::header(string)? {
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

            // Cannot return from function until after elements.drain() is called.
            let object: Result<Poll<Object>, _> = match content_length {
                None => {
                    length = header_len;
                    Err(ComposeMessageMisstep::MissingContentLength)
                }
                Some(content_length) => {
                    #[allow(clippy::option_if_let_else)]
                    // False trigger. See https://github.com/rust-lang/rust-clippy/issues/5822.
                    if let Some(total_len) = header_len.checked_add(content_length) {
                        if elements.len() < total_len {
                            Ok(Poll::Pending)
                        } else if let Some(content) = string.get(header_len..total_len) {
                            length = total_len;
                            serde_json::from_str(content)
                                .map_err(ComposeMessageMisstep::from)
                                .map(Poll::Ready)
                        } else {
                            length = header_len;
                            Err(ComposeMessageMisstep::InvalidContentLength)
                        }
                    } else {
                        length = header_len;
                        Err(ComposeMessageMisstep::InvalidContentLength)
                    }
                }
            };

            drop(elements.drain(..length));
            object?.map(Self::from)
        } else {
            Poll::Pending
        }
    }
}

impl Display for Message {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.content)
    }
}

impl From<Object> for Message {
    fn from(value: Object) -> Self {
        Self { content: value }
    }
}

impl From<RequestObject> for Message {
    fn from(request_obj: RequestObject) -> Self {
        Self::from(Object::from(request_obj))
    }
}

impl From<ResponseObject> for Message {
    fn from(response_obj: ResponseObject) -> Self {
        Self::from(Object::from(response_obj))
    }
}

impl From<Message> for Kind {
    fn from(message: Message) -> Self {
        message.content.into()
    }
}

impl From<Message> for Product<Message, ErrorMessage> {
    fn from(message: Message) -> Self {
        Self::Output(message)
    }
}

// Used to be able to implement Failure on DisassembleFrom<Message>::Error.
/// Error disassembling message.
#[derive(Debug, thiserror::Error)]
pub enum DisassembleMessageFault {
    /// Error serializing message.
    #[error(transparent)]
    Serialize(#[from] serde_json::Error),
}

impl Flaws for DisassembleMessageFault {
    type Insufficiency = Flawless;
    type Defect = Self;
}

impl conventus::DisassembleFrom<Message> for u8 {
    type Error = DisassembleMessageFault;

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
#[derive(Debug, thiserror::Error)]
pub enum ComposeMessageMisstep {
    /// Unable to find end of header.
    #[error("")]
    InvalidHeaderEnd,
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
    InvalidContent(#[from] serde_json::Error),
}

/// Failed to create `Client`.
#[derive(Debug, thiserror::Error)]
pub enum CreateToolError {
    /// Failed to create server process.
    #[error(transparent)]
    CreateProcess(#[from] std::io::Error),
    /// Failed to insert request.
    #[error(transparent)]
    InsertRequest(#[from] InsertRequestError),
}
