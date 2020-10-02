//! A Language Server Protocol translator for clients.
#![allow(clippy::unreachable)] // Unavoidable for Enum derivation.
mod json_rpc;
mod lsp;

use {
    conventus::{AssembleFailure, AssembleFrom},
    core::{
        cell::{Cell, RefCell},
        convert::TryInto,
    },
    enum_map::{enum_map, Enum},
    fehler::{throw, throws},
    json_rpc::{Id, Method, Object, Params, Success},
    log::{error, trace},
    lsp::{
        AssembleMessageError, Message, ServerMessage, ServerNotification, ServerRequest,
        ServerResponse, UnknownServerMessageFailure,
    },
    lsp_types::{
        ClientCapabilities, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
        InitializeParams, InitializeResult, Registration, RegistrationParams,
        SynchronizationCapability, TextDocumentClientCapabilities, TextDocumentIdentifier,
        TextDocumentItem, Url,
    },
    market::{
        io::{Reader, WriteError},
        process::{CreateProcessError, Process, WaitProcessError, Waiter},
        sync::{Actuator, Releaser},
        ClosedMarketError, ConsumeCompositeError, ConsumeFailure, Consumer, Never, PermanentQueue,
        ProduceFailure, Producer,
    },
    parse_display::Display as ParseDisplay,
    serde_json::{Number, Value},
    std::{
        fmt::{self, Display},
        process::{self, Command, ExitStatus},
        rc::Rc,
        sync::Arc,
        thread::{self, JoinHandle},
    },
    thiserror::Error as ThisError,
};

/// The languages supported by docuglot.
#[derive(Clone, Copy, Debug, Enum, ParseDisplay, PartialEq)]
#[display(style = "lowercase")]
pub enum Language {
    /// Rust.
    Rust,
    /// Plain text.
    Plaintext,
}

/// Manages all of the `Translator`s.
#[derive(Debug)]
pub struct Tongue {
    /// The thread of Tongue.
    thread: Option<JoinHandle<()>>,
    /// Triggers the Tongue thread to join.
    join_actuator: Actuator,
    /// Outputs from the Translators.
    outputs: Arc<PermanentQueue<ClientStatement>>,
    /// Inputs to the Translators.
    inputs: Arc<PermanentQueue<ServerStatement>>,
    /// The statuses of the Translators.
    statuses: Arc<PermanentQueue<ExitStatus>>,
}

impl Tongue {
    /// Creates a new `Tongue`.
    #[inline]
    #[must_use]
    pub fn new(root_dir: &Url) -> Self {
        let (join_actuator, releaser) = market::sync::trigger();
        let dir = root_dir.clone();
        let outputs = Arc::new(PermanentQueue::new());
        let shared_outputs = Arc::clone(&outputs);
        let inputs = Arc::new(PermanentQueue::new());
        let shared_inputs = Arc::clone(&inputs);
        let statuses = Arc::new(PermanentQueue::new());
        let shared_statuses = Arc::clone(&statuses);

        Self {
            join_actuator,
            thread: Some(thread::spawn(move || {
                if let Err(error) = Self::thread(
                    &dir,
                    &releaser,
                    &shared_outputs,
                    &shared_inputs,
                    &shared_statuses,
                ) {
                    error!("tongue thread error: {}", error);
                }
            })),
            outputs,
            inputs,
            statuses,
        }
    }

    /// The main thread of a Tongue.
    #[throws(TranslationError)]
    fn thread(
        root_dir: &Url,
        releaser: &Releaser,
        outputs: &Arc<PermanentQueue<ClientStatement>>,
        inputs: &Arc<PermanentQueue<ServerStatement>>,
        statuses: &Arc<PermanentQueue<ExitStatus>>,
    ) {
        let rust_translator = Rc::new(RefCell::new(Translator::new(
            Client::new(Command::new("rust-analyzer"))?,
            root_dir.clone(),
        )));
        // TODO: Currently plaintext_translator is a hack to deal with all files that do not have a known language. Ideally, this would run its own language server.
        let plaintext_translator = Rc::new(RefCell::new(Translator::new(
            Client::new(Command::new("echo"))?,
            root_dir.clone(),
        )));
        plaintext_translator.borrow_mut().state = State::WaitingExit;
        let translators = enum_map! {
            Language::Rust => Rc::clone(&rust_translator),
            Language::Plaintext => Rc::clone(&plaintext_translator),
        };

        while !translators
            .values()
            .map(|t| t.borrow().state == State::WaitingExit)
            .all(|x| x)
        {
            let will_shutdown = releaser.consume().is_ok();

            for output in outputs.consume_all()? {
                #[allow(clippy::indexing_slicing)]
                // translators is an EnumMap so index will not panic.
                translators[output.language()]
                    .borrow_mut()
                    .send_message(output.into())?;
            }

            for (_, translator) in &translators {
                if let Some(input) = translator.borrow_mut().translate()? {
                    inputs.produce(input)?;
                }

                translator.borrow().log_errors();

                if will_shutdown {
                    translator.borrow_mut().shutdown()?;
                }
            }
        }

        for (_, translator) in translators {
            statuses.produce(translator.borrow().waiter().demand()?)?;
        }
    }

    /// Joins the thread.
    #[allow(clippy::expect_used)] // Join API does not return a Result.
    #[inline]
    pub fn join(&mut self) {
        if let Some(thread) = self.thread.take() {
            self.join_actuator
                .produce(())
                .expect("Failed to trigger joining of Tongue");

            thread.join().expect("Failed to join Tongue");
        }
    }
}

impl Consumer for Tongue {
    type Good = ServerStatement;
    type Error = <PermanentQueue<ServerStatement> as Consumer>::Error;

    #[inline]
    #[throws(ConsumeFailure<Self::Error>)]
    fn consume(&self) -> Self::Good {
        self.inputs.consume()?
    }
}

impl Producer for Tongue {
    type Good = ClientStatement;
    type Error = <PermanentQueue<ClientStatement> as Producer>::Error;

    #[inline]
    #[throws(ProduceFailure<Self::Error>)]
    fn produce(&self, good: Self::Good) {
        self.outputs.produce(good)?
    }
}

/// A statement from the server.
#[derive(Clone, Copy, Debug, ParseDisplay)]
#[display(style = "CamelCase")]
pub enum ServerStatement {
    /// The server exited.
    Exit,
}

/// A statement from the client.
#[derive(Debug, ParseDisplay)]
#[display("")]
pub enum ClientStatement {
    /// The tool opened `doc`.
    OpenDoc {
        /// The document that was opened.
        doc: TextDocumentItem,
    },
    /// The tool closed `doc`.
    CloseDoc {
        /// The document that was closed.
        doc: TextDocumentIdentifier,
    },
}

impl ClientStatement {
    /// Creates a `didOpen` `ClientStatement`.
    #[inline]
    #[must_use]
    pub const fn open_doc(doc: TextDocumentItem) -> Self {
        Self::OpenDoc { doc }
    }

    /// Creates a `didClose` `ClientStatement`.
    #[inline]
    #[must_use]
    pub const fn close_doc(doc: TextDocumentIdentifier) -> Self {
        Self::CloseDoc { doc }
    }

    /// Returns the language of `self`.
    #[allow(clippy::unused_self)] // Will require self in the future.
    const fn language(&self) -> Language {
        Language::Rust
    }
}

impl From<ClientStatement> for ClientMessage {
    #[inline]
    fn from(value: ClientStatement) -> Self {
        match value {
            ClientStatement::OpenDoc { doc } => {
                Self::Notification(ClientNotification::OpenDoc(DidOpenTextDocumentParams {
                    text_document: doc,
                }))
            }
            ClientStatement::CloseDoc { doc } => {
                Self::Notification(ClientNotification::CloseDoc(DidCloseTextDocumentParams {
                    text_document: doc,
                }))
            }
        }
    }
}

/// An error message.
#[derive(Clone, Debug)]
pub struct ErrorMessage {
    /// The message.
    line: String,
}

/// An error while composing an error message.
#[derive(Clone, Copy, Debug, ThisError)]
#[error("Error while composing error message")]
pub struct ErrorMessageCompositionError;

impl AssembleFrom<u8> for ErrorMessage {
    type Error = ErrorMessageCompositionError;

    #[inline]
    #[throws(AssembleFailure<Self::Error>)]
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
                throw!(AssembleFailure::Incomplete);
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

/// An error during translation.
#[derive(Debug, ThisError)]
pub enum TranslationError {
    /// Failure while transmitting a client message.
    #[error(transparent)]
    Transmission(#[from] ProduceFailure<WriteError<Message>>),
    /// Failure while receiving a server message.
    #[error(transparent)]
    Reception(#[from] ConsumeServerMessageError),
    /// Failure while creating client.
    #[error(transparent)]
    CreateClient(#[from] CreateClientError),
    /// Failure while waiting for process.
    #[error(transparent)]
    Wait(#[from] WaitProcessError),
    /// Failure while collecting outputs.
    #[error(transparent)]
    CollectOutputs(#[from] ConsumeFailure<<PermanentQueue<ClientMessage> as Consumer>::Error>),
    /// Attempted to process an event in an invalid state.
    #[error("Invalid state: Cannot {0} while {1}")]
    InvalidState(Box<Event>, State),
    /// Failed to store output.
    #[error(transparent)]
    Storage(#[from] ProduceFailure<Never>),
    /// An error that will never occur.
    #[error(transparent)]
    Never(#[from] Never),
}

/// Describes events that the client must process.
#[derive(Debug, ParseDisplay, PartialEq)]
pub enum Event {
    /// The tool wants to send a message to the server.
    #[display("send message")]
    SendMessage(ClientMessage),
    /// THe server has completed initialization.
    #[display("process initialization")]
    Initialized(InitializeResult),
    /// The server has registered capabilities with the client.
    #[display("register capability")]
    RegisterCapability(Id, RegistrationParams),
    /// The server has completed shutdown.
    #[display("complete shutdown")]
    CompletedShutdown,
    /// The tool wants to kill the server.
    #[display("exit")]
    Exit,
}

/// Describes the state of the client.
#[derive(Clone, Debug, ParseDisplay, PartialEq)]
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
        registrations: Vec<Registration>,
    },
    /// Waiting for the server to confirm shutdown.
    #[display("waiting shutdown")]
    WaitingShutdown,
    /// Waiting for the server to exit.
    #[display("waiting exit")]
    WaitingExit,
}

/// Manages the client of a language server.
pub(crate) struct Translator {
    /// The client.
    client: Client,
    /// The state.
    state: State,
}

impl Translator {
    /// Creates a new `Translator`.
    const fn new(client: Client, root_dir: Url) -> Self {
        Self {
            client,
            state: State::Uninitialized { root_dir },
        }
    }

    /// Sends `message` to the server.
    ///
    /// If not done immediately, the client will send the message as soon as possible.
    #[throws(TranslationError)]
    fn send_message(&mut self, message: ClientMessage) {
        self.process(Event::SendMessage(message))?
    }

    /// Translates the input consumed by the client to a `ServerStatement`.
    #[throws(TranslationError)]
    fn translate(&mut self) -> Option<ServerStatement> {
        match self.client.consume() {
            Ok(message) => {
                match message {
                    ServerMessage::Request { id, request } => match request {
                        ServerRequest::RegisterCapability(registration) => {
                            self.process(Event::RegisterCapability(id, registration))?;
                        }
                    },
                    ServerMessage::Response(response) => match response {
                        ServerResponse::Initialize(initialize) => {
                            self.process(Event::Initialized(initialize))?;
                        }
                        ServerResponse::Shutdown => {
                            self.process(Event::CompletedShutdown)?;
                        }
                    },
                    ServerMessage::Notification(notification) => match notification {
                        ServerNotification::PublishDiagnostics(_diagnostics) => {
                            // TODO: Send diagnostics to tool.
                        }
                    },
                }
            }
            Err(error) => {
                if let ConsumeFailure::Error(failure) = error {
                    throw!(TranslationError::from(failure));
                }
            }
        }

        None
    }

    /// Processes `event`.
    #[throws(TranslationError)]
    fn process(&mut self, event: Event) {
        match &self.state {
            State::Uninitialized { root_dir } => match event {
                Event::SendMessage(message) => {
                    self.client.initialize(root_dir)?;
                    self.state = State::WaitingInitialization {
                        messages: vec![message],
                    }
                }
                Event::Initialized(_)
                | Event::CompletedShutdown
                | Event::RegisterCapability(..) => {
                    throw!(TranslationError::InvalidState(
                        Box::new(event),
                        self.state.clone()
                    ));
                }
                Event::Exit => {
                    self.client
                        .produce(ClientMessage::Notification(ClientNotification::Exit))?;
                    self.state = State::WaitingExit
                }
            },
            State::WaitingInitialization { messages } => match event {
                Event::SendMessage(message) => {
                    let mut new_messages = messages.clone();
                    new_messages.push(message);
                    self.state = State::WaitingInitialization {
                        messages: new_messages,
                    }
                }
                Event::Initialized(server_state) => {
                    self.client
                        .produce(ClientMessage::Notification(ClientNotification::Initialized))?;

                    for message in messages {
                        self.client.produce(message.clone())?;
                    }

                    self.state = State::Running {
                        server_state: Box::new(server_state),
                        registrations: Vec::new(),
                    }
                }
                Event::CompletedShutdown | Event::RegisterCapability(..) => {
                    throw!(TranslationError::InvalidState(
                        Box::new(event),
                        self.state.clone()
                    ));
                }
                Event::Exit => {
                    // TODO: Figure out how to handle this case.
                }
            },
            State::Running {
                server_state,
                registrations,
            } => match event {
                Event::SendMessage(message) => {
                    self.client.produce(message)?;
                }
                Event::RegisterCapability(id, mut register) => {
                    let mut new_registrations = registrations.clone();
                    new_registrations.append(&mut register.registrations);
                    self.state = State::Running {
                        server_state: server_state.clone(),
                        registrations: new_registrations,
                    };
                    self.client.produce(ClientMessage::Response {
                        id,
                        response: ClientResponse::RegisterCapability,
                    })?;
                }
                Event::Initialized(_) | Event::CompletedShutdown => {
                    throw!(TranslationError::InvalidState(
                        Box::new(event),
                        self.state.clone()
                    ));
                }
                Event::Exit => {
                    self.client
                        .produce(ClientMessage::Request(ClientRequest::Shutdown))?;
                    self.state = State::WaitingShutdown;
                }
            },
            State::WaitingShutdown => match event {
                Event::SendMessage(_)
                | Event::Initialized(_)
                | Event::Exit
                | Event::RegisterCapability(..) => {
                    throw!(TranslationError::InvalidState(
                        Box::new(event),
                        self.state.clone()
                    ));
                }
                Event::CompletedShutdown => {
                    self.client
                        .produce(ClientMessage::Notification(ClientNotification::Exit))?;
                    self.state = State::WaitingExit;
                }
            },
            State::WaitingExit => self.while_waiting_exit(event)?,
        }
    }

    /// Processes `event` while in [`State::WaitingExit`].
    #[throws(TranslationError)]
    fn while_waiting_exit(&self, event: Event) {
        if event != Event::Exit {
            throw!(TranslationError::InvalidState(
                Box::new(event),
                self.state.clone(),
            ));
        }
    }

    /// Logs all messages that have currently been received on stderr.
    fn log_errors(&self) {
        match self.client.stderr().consume_all() {
            Ok(messages) => {
                for message in messages {
                    error!("lsp stderr: {}", message);
                }
            }
            Err(error) => {
                error!("error logger: {}", error);
            }
        }
    }

    /// Shuts down the client.
    #[throws(TranslationError)]
    fn shutdown(&mut self) {
        self.process(Event::Exit)?;
    }

    /// The client's `Waiter`.
    const fn waiter(&self) -> &Waiter {
        self.client.waiter()
    }
}

/// A client to a language server.
pub(crate) struct Client {
    /// The server.
    server: Process<Message, Message, ErrorMessage>,
    /// The `Id` of the next request.
    next_id: Cell<u64>,
}

impl Client {
    /// Creates a new [`Client`] for `language`.
    #[throws(CreateClientError)]
    fn new(command: Command) -> Self {
        Self {
            server: Process::new(command)?,
            next_id: Cell::new(1),
        }
    }

    /// Sends an initialize request to the server.
    #[throws(TranslationError)]
    fn initialize(&self, root_dir: &Url) {
        #[allow(deprecated)] // InitializeParams.root_path is required.
        self.produce(ClientMessage::Request(ClientRequest::Initialize(
            InitializeParams {
                process_id: Some(u64::from(process::id())),
                root_path: None,
                root_uri: Some(root_dir.clone()),
                initialization_options: None,
                capabilities: ClientCapabilities {
                    workspace: None,
                    text_document: Some(TextDocumentClientCapabilities {
                        synchronization: Some(SynchronizationCapability {
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
                        document_symbol: None,
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
                    }),
                    window: None,
                    experimental: None,
                },
                trace: None,
                workspace_folders: None,
                client_info: None,
            },
        )))?;
    }

    /// The server's `Waiter`.
    const fn waiter(&self) -> &Waiter {
        self.server.waiter()
    }

    /// The server's stderr.
    const fn stderr(&self) -> &Reader<ErrorMessage> {
        self.server.stderr()
    }

    /// Returns the `Id` of the next request sent by `self`.
    fn next_id(&self) -> Id {
        let id = self.next_id.get();
        self.next_id.set(id.wrapping_add(1));
        Id::Num(Number::from(id))
    }
}

impl Consumer for Client {
    type Good = ServerMessage;
    type Error = ConsumeServerMessageError;

    #[throws(ConsumeFailure<Self::Error>)]
    fn consume(&self) -> Self::Good {
        let good = self
            .server
            .consume()
            .map_err(ConsumeFailure::map_into)?
            .try_into()
            .map_err(|error| ConsumeFailure::Error(Self::Error::from(error)))?;
        trace!("LSP Rx: {}", good);
        good
    }
}

impl Producer for Client {
    type Good = ClientMessage;
    type Error = WriteError<Message>;

    #[throws(ProduceFailure<Self::Error>)]
    fn produce(&self, good: Self::Good) {
        trace!("LSP Tx: {}", good);
        let message = Message::from(match good {
            ClientMessage::Response { id, response } => Object::response(id, &response),
            ClientMessage::Request(request) => Object::request(self.next_id(), &request),
            ClientMessage::Notification(notification) => Object::notification(&notification),
        });

        self.server
            .produce(message)
            .map_err(ProduceFailure::map_into)?
    }
}

/// A message to the language server.
#[derive(Clone, Debug, PartialEq)]
pub enum ClientMessage {
    /// A request.
    Request(ClientRequest),
    /// A response.
    Response {
        /// The id of the matching request.
        id: Id,
        /// The response.
        response: ClientResponse,
    },
    /// A notification.
    Notification(ClientNotification),
}

impl Display for ClientMessage {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Request(request) => format!("{}: {}", "Request", request),
                Self::Response { response, .. } => format!("{}: {}", "Response", response),
                Self::Notification(notification) => format!("{}: {}", "Notification", notification),
            }
        )
    }
}

/// A request from the client.
#[derive(Clone, Debug, ParseDisplay, PartialEq)]
pub enum ClientRequest {
    /// The client is initializing the server.
    #[display("Initialize w/ {0:?}")]
    Initialize(InitializeParams),
    /// The client is shutting down the server.
    Shutdown,
}

impl Method for ClientRequest {
    fn method(&self) -> String {
        match self {
            Self::Initialize(_) => "initialize",
            Self::Shutdown => "shutdown",
        }
        .to_string()
    }

    fn params(&self) -> Params {
        match self {
            Self::Initialize(params) => {
                #[allow(clippy::expect_used)] // InitializeParams can be serialized to JSON.
                Params::from(
                    serde_json::to_value(params)
                        .expect("Converting InitializeParams to JSON Value"),
                )
            }
            Self::Shutdown => Params::None,
        }
    }
}

/// A notification from the client.
#[derive(Clone, Debug, ParseDisplay, PartialEq)]
pub enum ClientNotification {
    /// The client received the server's initialization response.
    Initialized,
    /// The client requests the server to exit.
    Exit,
    /// The client opened a document.
    #[display("OpenDoc w/ {0:?}")]
    OpenDoc(DidOpenTextDocumentParams),
    /// The client closed a document.
    #[display("CloseDoc w/ {0:?}")]
    CloseDoc(DidCloseTextDocumentParams),
}

impl Method for ClientNotification {
    fn method(&self) -> String {
        match self {
            Self::Initialized => "initialized",
            Self::Exit => "exit",
            Self::OpenDoc(_) => "textDocument/didOpen",
            Self::CloseDoc(_) => "textDocument/didClose",
        }
        .to_string()
    }

    fn params(&self) -> Params {
        match self {
            Self::Initialized | Self::Exit => Params::None,
            Self::OpenDoc(params) => {
                #[allow(clippy::expect_used)]
                // DidOpenTextDocumentParams can be serialized to JSON.
                Params::from(
                    serde_json::to_value(params)
                        .expect("Converting DidOpenTextDocumentParams to JSON Value"),
                )
            }
            Self::CloseDoc(params) => {
                #[allow(clippy::expect_used)]
                // DidCloseTextDocumentParams can be serialized to JSON.
                Params::from(
                    serde_json::to_value(params)
                        .expect("Converting DidCloseTextDocumentParams to JSON Value"),
                )
            }
        }
    }
}

/// A response from the client.
#[derive(Clone, Copy, Debug, ParseDisplay, PartialEq)]
pub enum ClientResponse {
    /// Confirms client registered capabilities.
    RegisterCapability,
}

impl Success for ClientResponse {
    fn result(&self) -> Value {
        match self {
            Self::RegisterCapability => Value::Null,
        }
    }
}

/// Failed to create `Client`.
#[derive(Debug, ThisError)]
pub enum CreateClientError {
    /// Failed to create server process.
    #[error(transparent)]
    CreateProcess(#[from] CreateProcessError),
}

/// Client failed to consume `ServerMessage`.
#[derive(Debug, ThisError)]
pub enum ConsumeServerMessageError {
    /// Client failed to consume Message from server.
    #[error(transparent)]
    Consume(#[from] ConsumeCompositeError<AssembleMessageError, ClosedMarketError>),
    /// Client failed to convert Message into ServerMessage.
    #[error(transparent)]
    UnknownServerMessage(#[from] UnknownServerMessageFailure),
}
