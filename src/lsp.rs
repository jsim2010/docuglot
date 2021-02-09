//! Implements the language server protocol functionality of docuglot.
// TODO: This should be moved to only occur on cur (functionality exists in master but not 1.46.0.
#![allow(clippy::wildcard_imports)] // cur is designed to use wildcard import.
use {
    crate::json_rpc::{
        self, InsertRequestError, Kind, MethodHandlers, Object, Outcome, Params,
        ProcessResponseError, Request, ResponseHandlers,
    },
    conventus::AssembleFrom,
    core::{
        fmt::{self, Display},
        str::Utf8Error,
    },
    fehler::{throw, throws},
    lsp_types::{
        DidCloseTextDocumentParams, DidOpenTextDocumentParams, DocumentSymbolClientCapabilities,
        DocumentSymbolParams, DocumentSymbolResponse, InitializeParams, InitializeResult,
        InitializedParams, RegistrationParams, TextDocumentSyncClientCapabilities, Url,
    },
    market::{
        channel::{WithdrawnDemandFault, WithdrawnSupplyFault},
        io::{ReadFault, WriteFault},
        process::Process,
        ConsumeFault, Consumer, Failure, ProduceFailure, Producer,
    },
    serde_json::Value,
    std::process::{self, Command},
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
        capabilities: lsp_types::ClientCapabilities {
            workspace: None,
            text_document: Some(lsp_types::TextDocumentClientCapabilities {
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

/// Describes events that the client must process.
#[derive(Debug, parse_display::Display)]
pub(crate) enum Event {
    /// The tool wants to send a message to the server.
    #[display("send message")]
    SendMessages(Vec<ClientMessage>),
    /// The tool encountered an error.
    #[display("error")]
    Error(TranslationError),
    /// The tool received a document symbol.
    #[display("")]
    DocumentSymbol(DocumentSymbolResponse),
}

/// An error during translation.
#[derive(Debug, ConsumeFault, thiserror::Error)]
pub enum TranslationError {
    /// Failure while transmitting a client message.
    #[error(transparent)]
    Transmission(#[from] WriteFault<Message>),
    /// Failure while receiving a server message.
    #[error(transparent)]
    Reception(#[from] ConsumeServerMessageError),
    /// Failure while creating client.
    #[error(transparent)]
    CreateClient(#[from] CreateClientError),
    /// Failure while waiting for process.
    #[error(transparent)]
    Wait(#[from] market::process::WaitFault),
    /// Attempted to consume on channel with no supply.
    #[error(transparent)]
    NoSupply(#[from] WithdrawnSupplyFault),
    /// Attempted to produce on channel with dropped [`Consumer`].
    #[error(transparent)]
    NoDemand(#[from] ProduceFailure<WithdrawnDemandFault>),
    /// An error serializing a message.
    #[error(transparent)]
    Serialize(#[from] serde_json::Error),
    /// An error reading the message.
    #[error(transparent)]
    Read(#[from] ReadFault<Message>),
    /// A JSON-RPC error.
    #[error(transparent)]
    ProcessResponse(#[from] ProcessResponseError),
    /// An invalid state.
    #[error("LSP client in invalid state: {0}")]
    InvalidState(State),
}

/// A message to the language server.
#[derive(Clone, Debug, parse_display::Display, PartialEq)]
pub enum ClientMessage {
    /// The client received the initialization.
    #[display("Initialized")]
    Initialized,
    /// The client is shutting down.
    #[display("Shutdown")]
    Shutdown,
    /// The client is exiting.
    #[display("Exit")]
    Exit,
    /// The client requests a document symbol.
    #[display("DocumentSymbol {0:?}")]
    DocumentSymbol(DocumentSymbolParams),
    /// The client opened a document.
    #[display("OpenDoc w/ {0:?}")]
    OpenDoc(DidOpenTextDocumentParams),
    /// The client closed a document.
    #[display("CloseDoc w/ {0:?}")]
    CloseDoc(DidCloseTextDocumentParams),
}

/// An error message.
#[derive(Clone, Debug)]
pub(crate) struct ErrorMessage {
    /// The message.
    line: String,
}

/// An error while composing an error message.
#[derive(Clone, Copy, ConsumeFault, Debug, thiserror::Error)]
#[error("Error while composing error message")]
pub(crate) struct ErrorMessageCompositionError;

impl AssembleFrom<u8> for ErrorMessage {
    type Error = ErrorMessageCompositionError;

    #[inline]
    #[throws(conventus::AssembleFailure<Self::Error>)]
    fn assemble_from(parts: &mut Vec<u8>) -> Self {
        if let Ok(s) = std::str::from_utf8_mut(parts) {
            if let Some(index) = s.find('\n') {
                let (l, remainder) = s.split_at_mut(index);
                let (_, new_parts) = remainder.split_at_mut(1);
                let line = (*l).to_string();
                *parts = new_parts.as_bytes().to_vec();

                Self { line }
            } else {
                // parts does not contain a new line.
                throw!(conventus::AssembleFailure::Incomplete);
            }
        } else {
            // parts has some invalid uft8.
            *parts = Vec::new();
            throw!(ErrorMessageCompositionError);
        }
    }
}

impl Display for ErrorMessage {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.line)
    }
}

/// Describes the state of the client.
#[derive(Clone, Debug, parse_display::Display, PartialEq)]
pub enum State {
    /// Server has not been initialized.
    #[display("uninitialized")]
    Uninitialized {
        /// The desired root directory of the server.
        root_dir: Url,
    },
    /// Waiting for the server to confirm initialization.
    #[display("waiting initialization")]
    WaitingInitialization {
        /// Messages to be sent after initialization is confirmed.
        messages: Vec<ClientMessage>,
    },
    /// Normal running state.
    #[display("running")]
    Running {
        /// The state of the server.
        server_state: Box<InitializeResult>,
        /// The registrations.
        registrations: Vec<lsp_types::Registration>,
    },
    /// Waiting for the server to confirm shutdown.
    #[display("waiting shutdown")]
    WaitingShutdown,
    /// Waiting for the server to exit.
    #[display("waiting exit")]
    WaitingExit,
}

/// The LSP client.
pub(crate) struct Tool {
    /// The LSP server process.
    lsp_server: Process<Message, Message, ErrorMessage>,
    /// The JSON-RPC client.
    rpc_client: json_rpc::Client<State, Event>,
    /// The JSON-RPC server.
    rpc_server: json_rpc::Server<State>,
    /// Defines the current state of `Self`.
    state: State,
}

impl Tool {
    /// Creates a new `Tool`.
    #[throws(CreateClientError)]
    pub(crate) fn new(command: Command, root_dir: Url) -> Self {
        Self {
            lsp_server: market::process::Process::new(command)?,
            state: State::Uninitialized { root_dir },
            rpc_client: json_rpc::Client::new(),
            rpc_server: json_rpc::Server::new(vec![RegistrationParams {
                registrations: vec![],
            }])?,
        }
    }

    /// Creates a new [`Tool`] that is already waiting to exit.
    #[throws(CreateClientError)]
    pub(crate) fn new_finished(command: Command) -> Self {
        Self {
            lsp_server: market::process::Process::new(command)?,
            state: State::WaitingExit,
            rpc_client: json_rpc::Client::new(),
            rpc_server: json_rpc::Server::new(vec![RegistrationParams {
                registrations: vec![],
            }])?,
        }
    }

    /// Transmits `messages` to the LSP server.
    #[throws(TranslationError)]
    pub(crate) fn transmit(&mut self, mut messages: Vec<ClientMessage>) {
        log::trace!("transmit {:?}", messages);
        match &mut self.state {
            State::Uninitialized { ref root_dir } => {
                self.lsp_server.input().produce(Message::from(Object::from(
                    self.rpc_client.request(&initialize_params(root_dir))?,
                )))?;
                self.state = State::WaitingInitialization { messages };
            }
            State::WaitingInitialization {
                messages: pending_messages,
            } => {
                pending_messages.append(&mut messages);
            }
            State::Running { .. } => {
                for message in messages {
                    self.lsp_server
                        .input()
                        .produce(Message::from(match message {
                            ClientMessage::Initialized => {
                                Object::from(self.rpc_client.request(&InitializedParams {})?)
                            }
                            ClientMessage::Shutdown => {
                                Object::from(self.rpc_client.request(&ShutdownParams)?)
                            }
                            ClientMessage::Exit => {
                                Object::from(self.rpc_client.request(&ExitParams {})?)
                            }
                            ClientMessage::DocumentSymbol(params) => {
                                Object::from(self.rpc_client.request(&params)?)
                            }
                            ClientMessage::OpenDoc(params) => {
                                Object::from(self.rpc_client.request(&params)?)
                            }
                            ClientMessage::CloseDoc(params) => {
                                Object::from(self.rpc_client.request(&params)?)
                            }
                        }))?;
                }
            }
            State::WaitingShutdown | State::WaitingExit => {
                throw!(TranslationError::InvalidState(self.state.clone()));
            }
        }
        log::trace!("end transmit");
    }

    /// Processes all receptions from LSP server.
    #[throws(TranslationError)]
    pub(crate) fn process_receptions(&mut self) -> Vec<Event> {
        let mut receptions = Vec::new();

        for good in self.lsp_server.output().goods() {
            if let Some(reception) = match good?.into() {
                Kind::Request(request_object) => {
                    if let Some(response) = self
                        .rpc_server
                        .process_request(&mut self.state, request_object)
                    {
                        self.lsp_server
                            .input()
                            .produce(Message::from(Object::from(response)))?;
                    }

                    None
                }
                Kind::Response(response) => self
                    .rpc_client
                    .process_response(&mut self.state, response)?,
            } {
                receptions.push(reception);
            }
        }

        receptions
    }

    /// Logs all messages that have currently been received on stderr.
    pub(crate) fn log_errors(&self) {
        for good in self.lsp_server.error().goods() {
            match good {
                Ok(message) => log::error!("lsp stderr: {}", message),
                Err(error) => log::error!("error logger: {}", error),
            }
        }
    }

    /// Returns the server process.
    pub(crate) const fn server(&self) -> &Process<Message, Message, ErrorMessage> {
        &self.lsp_server
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
            |mut state, value| {
                Some(match &mut state {
                    State::Uninitialized { .. }
                    | State::Running { .. }
                    | State::WaitingShutdown
                    | State::WaitingExit => {
                        Ok(Event::Error(TranslationError::InvalidState(state.clone())))
                    }
                    State::WaitingInitialization { messages } => {
                        match serde_json::from_value::<InitializeResult>(value) {
                            Ok(initialize_result) => {
                                let mut m = messages.clone();
                                m.push(ClientMessage::Initialized);

                                *state = State::Running {
                                    server_state: Box::new(initialize_result),
                                    registrations: Vec::new(),
                                };

                                Ok(Event::SendMessages(m))
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
            |mut state, _| {
                Some(match &mut state {
                    State::Uninitialized { .. }
                    | State::Running { .. }
                    | State::WaitingInitialization { .. }
                    | State::WaitingExit => {
                        Ok(Event::Error(TranslationError::InvalidState(state.clone())))
                    }
                    State::WaitingShutdown => {
                        *state = State::WaitingExit;
                        Ok(Event::SendMessages(vec![ClientMessage::Exit]))
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
            Some(|mut state, params| match &mut state {
                State::Running { registrations, .. } => {
                    match serde_json::from_value::<Self>(params.into()) {
                        Ok(mut register) => {
                            registrations.append(&mut register.registrations);
                            Outcome::Result(Value::Null)
                        }
                        Err(error) => Outcome::invalid_params(&error),
                    }
                }
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
    #[throws(conventus::AssembleFailure<AssembleMessageError>)]
    fn assemble_from(parts: &mut Vec<u8>) -> Self {
        // TODO: This would probably be simpler with a regex.
        let mut length = 0;

        let string = std::str::from_utf8(parts).map_err(Self::Error::from)?;
        let header = Self::header(string).ok_or(conventus::AssembleFailure::Incomplete)?;
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
                Err(conventus::AssembleFailure::Error(
                    AssembleMessageError::MissingContentLength,
                ))
            }
            Some(content_length) => {
                #[allow(clippy::option_if_let_else)]
                // False trigger. See https://github.com/rust-lang/rust-clippy/issues/5822.
                if let Some(total_len) = header_len.checked_add(content_length) {
                    if parts.len() < total_len {
                        Err(conventus::AssembleFailure::Incomplete)
                    } else if let Some(content) = string.get(header_len..total_len) {
                        length = total_len;
                        serde_json::from_str(content).map_err(|error| {
                            conventus::AssembleFailure::Error(AssembleMessageError::from(error))
                        })
                    } else {
                        length = header_len;
                        Err(conventus::AssembleFailure::Error(
                            AssembleMessageError::InvalidContentLength,
                        ))
                    }
                } else {
                    length = header_len;
                    Err(conventus::AssembleFailure::Error(
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

impl From<Message> for Kind {
    fn from(message: Message) -> Self {
        message.content.into()
    }
}

impl From<Object> for Message {
    #[inline]
    fn from(value: Object) -> Self {
        Self { content: value }
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

impl Failure for DisassembleMessageFault {
    type Fault = Self;
}

#[allow(clippy::use_self)] // False positive on format!.
impl conventus::DisassembleFrom<Message> for u8 {
    type Error = DisassembleMessageFault;

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
#[derive(ConsumeFault, Debug, thiserror::Error)]
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
    InvalidContent(#[from] serde_json::Error),
}

/// Failed to create `Client`.
#[derive(Debug, thiserror::Error)]
pub enum CreateClientError {
    /// Failed to create server process.
    #[error(transparent)]
    CreateProcess(#[from] market::process::CreateProcessError),
    /// Failed to insert request.
    #[error(transparent)]
    InsertRequest(#[from] InsertRequestError),
}

/// Client failed to consume `lsp::ServerMessage`.
#[derive(ConsumeFault, Debug, thiserror::Error)]
pub enum ConsumeServerMessageError {
    /// Client failed to consume lsp::Message from server.
    #[error(transparent)]
    Consume(#[from] ReadFault<Message>),
}
