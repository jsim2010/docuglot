//! A Language Server Protocol translator for clients.
#![allow(clippy::unreachable)] // Unavoidable for Enum derivation.
#![allow(clippy::pattern_type_mismatch)] // Marks enums as errors.
mod json_rpc;
mod lsp;

use {
    core::{
        cell::{Cell, RefCell},
        convert::TryInto,
        fmt::{self, Display},
    },
    fehler::{throw, throws},
    log::{error, trace},
    lsp::Message,
    lsp_types::TextDocumentSyncClientCapabilities,
    market::{
        channel::{
            create, Crossbeam, CrossbeamConsumer, CrossbeamProducer, Size, Structure, Style,
            WithdrawnDemand, WithdrawnSupply,
        },
        io::{ReadFault, Reader, WriteFault},
        process::Process,
        sync::create_lock,
        ConsumeFailure, ConsumeFault, Consumer, ProduceFailure, Producer,
    },
    std::{
        process::{self, Command, ExitStatus},
        rc::Rc,
        thread::{self, JoinHandle},
    },
};

/// The languages supported by docuglot.
#[derive(Clone, Copy, Debug, enum_map::Enum, parse_display::Display, PartialEq)]
#[display(style = "lowercase")]
pub enum Language {
    /// Rust.
    Rust,
    /// Plain text.
    Plaintext,
}

/// Initialize an instance of a [`Tongue`].
#[must_use]
#[inline]
pub fn init(
    root_dir: &lsp_types::Url,
) -> (
    CrossbeamProducer<ClientStatement>,
    CrossbeamConsumer<ServerStatement>,
    Tongue,
) {
    let dir = root_dir.clone();
    let (trigger, hammer) = create_lock();
    let (transmission_producer, transmission_consumer) =
        create::<Crossbeam<ClientStatement>>(Structure::BilateralMonopoly, Size::Infinite);
    let (reception_producer, reception_consumer) =
        create::<Crossbeam<ServerStatement>>(Structure::BilateralMonopoly, Size::Infinite);
    let (status_producer, status_consumer) =
        create::<Crossbeam<ExitStatus>>(Structure::BilateralMonopoly, Size::Infinite);

    (
        transmission_producer,
        reception_consumer,
        Tongue {
            joiner: trigger,
            thread: thread::spawn(move || {
                if let Err(error) = Tongue::thread(
                    &dir,
                    &hammer,
                    &transmission_consumer,
                    &reception_producer,
                    &status_producer,
                ) {
                    error!("tongue thread error: {}", error);
                }
            }),
            status_consumer,
        },
    )
}

/// Manages all of the `Translator`s.
#[derive(Debug)]
pub struct Tongue {
    /// The thread of Tongue.
    thread: JoinHandle<()>,
    /// Triggers the Tongue thread to join.
    joiner: market::sync::Trigger,
    /// The statuses of the Translators.
    status_consumer: <Crossbeam<ExitStatus> as Style>::Consumer,
}

impl Tongue {
    /// The main thread of a Tongue.
    #[throws(TranslationError)]
    fn thread(
        root_dir: &lsp_types::Url,
        hammer: &market::sync::Hammer,
        transmission_consumer: &<Crossbeam<ClientStatement> as Style>::Consumer,
        reception_producer: &<Crossbeam<ServerStatement> as Style>::Producer,
        status_producer: &<Crossbeam<ExitStatus> as Style>::Producer,
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
        let translators = enum_map::enum_map! {
            Language::Rust => Rc::clone(&rust_translator),
            Language::Plaintext => Rc::clone(&plaintext_translator),
        };

        while !translators
            .values()
            .map(|t| t.borrow().state == State::WaitingExit)
            .all(|x| x)
        {
            let will_shutdown = hammer.consume().is_ok();

            for transmission in transmission_consumer.consume_all()? {
                #[allow(clippy::indexing_slicing)]
                // translators is an EnumMap so index will not panic.
                translators[transmission.language()]
                    .borrow_mut()
                    .send_message(transmission.into())?;
            }

            for (_, translator) in &translators {
                if let Some(input) = translator.borrow_mut().translate()? {
                    reception_producer.produce(input)?;
                }

                translator.borrow().log_errors();

                if will_shutdown {
                    translator.borrow_mut().shutdown()?;
                }
            }
        }

        for (_, translator) in translators {
            status_producer.produce(translator.borrow().server().demand()?)?;
        }
    }

    /// Joins the thread.
    #[inline]
    pub fn join(&self) {
        #[allow(clippy::unwrap_used)] // Trigger::produce() returns Result<_, Infallible>.
        self.joiner.produce(()).unwrap();

        // TODO: This appears to cause an error currently.
        //for _ in 0..2 {
        //    self.status_consumer.consume().expect("Failed to finish translator");
        //}
    }
}

/// A statement from the server.
#[derive(Clone, Copy, Debug, parse_display::Display)]
#[display(style = "CamelCase")]
pub enum ServerStatement {
    /// The server exited.
    Exit,
}

/// A statement from the client.
#[derive(Debug, parse_display::Display)]
#[display("")]
pub enum ClientStatement {
    /// The tool opened `doc`.
    OpenDoc {
        /// The document that was opened.
        doc: lsp_types::TextDocumentItem,
    },
    /// The tool closed `doc`.
    CloseDoc {
        /// The document that was closed.
        doc: lsp_types::TextDocumentIdentifier,
    },
}

impl ClientStatement {
    /// Creates a `didOpen` `ClientStatement`.
    #[inline]
    #[must_use]
    pub const fn open_doc(doc: lsp_types::TextDocumentItem) -> Self {
        Self::OpenDoc { doc }
    }

    /// Creates a `didClose` `ClientStatement`.
    #[inline]
    #[must_use]
    pub const fn close_doc(doc: lsp_types::TextDocumentIdentifier) -> Self {
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
            ClientStatement::OpenDoc { doc } => Self::Notification(ClientNotification::OpenDoc(
                lsp_types::DidOpenTextDocumentParams { text_document: doc },
            )),
            ClientStatement::CloseDoc { doc } => Self::Notification(ClientNotification::CloseDoc(
                lsp_types::DidCloseTextDocumentParams { text_document: doc },
            )),
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
#[derive(Clone, Copy, Debug, thiserror::Error)]
#[error("Error while composing error message")]
pub struct ErrorMessageCompositionError;

impl conventus::AssembleFrom<u8> for ErrorMessage {
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

/// An error during translation.
#[derive(Debug, thiserror::Error)]
pub enum TranslationError {
    /// Failure while transmitting a client message.
    #[error(transparent)]
    Transmission(#[from] ProduceFailure<WriteFault<Message>>),
    /// Failure while receiving a server message.
    #[error(transparent)]
    Reception(#[from] ConsumeServerMessageError),
    /// Failure while creating client.
    #[error(transparent)]
    CreateClient(#[from] CreateClientError),
    /// Failure while waiting for process.
    #[error(transparent)]
    Wait(#[from] market::process::WaitFault),
    /// Attempted to process an event in an invalid state.
    #[error("Invalid state: Cannot {0} while {1}")]
    InvalidState(Box<Event>, State),
    /// Attempted to consume on channel with no supply.
    #[error(transparent)]
    NoSupply(#[from] WithdrawnSupply),
    /// Attempted to produce on channel with dropped [`Consumer`].
    #[error(transparent)]
    NoDemand(#[from] ProduceFailure<WithdrawnDemand>),
}

/// Describes events that the client must process.
#[derive(Debug, parse_display::Display, PartialEq)]
pub enum Event {
    /// The tool wants to send a message to the server.
    #[display("send message")]
    SendMessage(ClientMessage),
    /// THe server has completed initialization.
    #[display("process initialization")]
    Initialized(lsp_types::InitializeResult),
    /// The server has registered capabilities with the client.
    #[display("register capability")]
    RegisterCapability(json_rpc::Id, lsp_types::RegistrationParams),
    /// The server has completed shutdown.
    #[display("complete shutdown")]
    CompletedShutdown,
    /// The tool wants to kill the server.
    #[display("exit")]
    Exit,
}

/// Describes the state of the client.
#[derive(Clone, Debug, parse_display::Display, PartialEq)]
pub enum State {
    /// Server has not been initialized.
    #[display("uninitialized")]
    Uninitialized {
        /// The desired root directory of the server.
        root_dir: lsp_types::Url,
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
        server_state: Box<lsp_types::InitializeResult>,
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

/// Manages the client of a language server.
pub(crate) struct Translator {
    /// The client.
    client: Client,
    /// The state.
    state: State,
}

impl Translator {
    /// Creates a new `Translator`.
    const fn new(client: Client, root_dir: lsp_types::Url) -> Self {
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
                    lsp::ServerMessage::Request { id, request } => match request {
                        lsp::ServerRequest::RegisterCapability(registration) => {
                            self.process(Event::RegisterCapability(id, registration))?;
                        }
                    },
                    lsp::ServerMessage::Response(response) => match response {
                        lsp::ServerResponse::Initialize(initialize) => {
                            self.process(Event::Initialized(initialize))?;
                        }
                        lsp::ServerResponse::Shutdown => {
                            self.process(Event::CompletedShutdown)?;
                        }
                    },
                    lsp::ServerMessage::Notification(notification) => match notification {
                        lsp::ServerNotification::PublishDiagnostics(_diagnostics) => {
                            // TODO: Send diagnostics to tool.
                        }
                    },
                }
            }
            Err(failure) => {
                if let market::ConsumeFailure::Fault(fault) = failure {
                    throw!(TranslationError::from(fault));
                }
            }
        }

        None
    }

    /// Processes `event`.
    #[throws(TranslationError)]
    fn process(&mut self, event: Event) {
        match self.state {
            State::Uninitialized { ref root_dir } => match event {
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
            State::WaitingInitialization { ref messages } => match event {
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
                ref server_state,
                ref registrations,
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

    /// Returns the server process.
    const fn server(&self) -> &Process<Message, Message, ErrorMessage> {
        self.client.server()
    }

    /// Shuts down the client.
    #[throws(TranslationError)]
    fn shutdown(&mut self) {
        self.process(Event::Exit)?;
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
            server: market::process::Process::new(command)?,
            next_id: Cell::new(1),
        }
    }

    /// Sends an initialize request to the server.
    #[throws(TranslationError)]
    fn initialize(&self, root_dir: &lsp_types::Url) {
        #[allow(deprecated)] // InitializeParams.root_path is required.
        self.produce(ClientMessage::Request(ClientRequest::Initialize(
            lsp_types::InitializeParams {
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
            },
        )))?;
    }

    /// Returns the server process.
    const fn server(&self) -> &Process<Message, Message, ErrorMessage> {
        &self.server
    }

    /// The server's stderr.
    const fn stderr(&self) -> &Reader<ErrorMessage> {
        self.server.error()
    }

    /// Returns the `Id` of the next request sent by `self`.
    fn next_id(&self) -> json_rpc::Id {
        let id = self.next_id.get();
        self.next_id.set(id.wrapping_add(1));
        json_rpc::Id::Num(serde_json::Number::from(id))
    }
}

impl Consumer for Client {
    type Good = lsp::ServerMessage;
    type Failure = ConsumeFailure<ConsumeServerMessageError>;

    #[throws(Self::Failure)]
    fn consume(&self) -> Self::Good {
        let good = self
            .server
            .output()
            .consume()
            .map_err(ConsumeFailure::map_fault)?
            .try_into()
            .map_err(|error| {
                market::ConsumeFailure::Fault(ConsumeServerMessageError::from(error))
            })?;
        trace!("LSP Rx: {}", good);
        good
    }
}

impl Producer for Client {
    type Good = ClientMessage;
    type Failure = ProduceFailure<WriteFault<Message>>;

    #[throws(Self::Failure)]
    fn produce(&self, good: Self::Good) {
        trace!("LSP Tx: {}", good);
        let message = lsp::Message::from(match good {
            ClientMessage::Response { id, response } => json_rpc::Object::response(id, &response),
            ClientMessage::Request(request) => json_rpc::Object::request(self.next_id(), &request),
            ClientMessage::Notification(notification) => {
                json_rpc::Object::notification(&notification)
            }
        });

        self.server.input().produce(message)?
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
        id: json_rpc::Id,
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
            match *self {
                Self::Request(ref request) => format!("{}: {}", "Request", request),
                Self::Response { ref response, .. } => format!("{}: {}", "Response", response),
                Self::Notification(ref notification) =>
                    format!("{}: {}", "Notification", notification),
            }
        )
    }
}

/// A request from the client.
#[derive(Clone, Debug, parse_display::Display, PartialEq)]
pub enum ClientRequest {
    /// The client is initializing the server.
    #[display("Initialize w/ {0:?}")]
    Initialize(lsp_types::InitializeParams),
    /// The client is shutting down the server.
    Shutdown,
}

impl json_rpc::Method for ClientRequest {
    fn method(&self) -> String {
        match *self {
            Self::Initialize(_) => "initialize",
            Self::Shutdown => "shutdown",
        }
        .to_string()
    }

    fn params(&self) -> json_rpc::Params {
        match *self {
            Self::Initialize(ref params) => {
                #[allow(clippy::expect_used)] // InitializeParams can be serialized to JSON.
                json_rpc::Params::from(
                    serde_json::to_value(params)
                        .expect("Converting InitializeParams to JSON Value"),
                )
            }
            Self::Shutdown => json_rpc::Params::None,
        }
    }
}

/// A notification from the client.
#[derive(Clone, Debug, parse_display::Display, PartialEq)]
pub enum ClientNotification {
    /// The client received the server's initialization response.
    Initialized,
    /// The client requests the server to exit.
    Exit,
    /// The client opened a document.
    #[display("OpenDoc w/ {0:?}")]
    OpenDoc(lsp_types::DidOpenTextDocumentParams),
    /// The client closed a document.
    #[display("CloseDoc w/ {0:?}")]
    CloseDoc(lsp_types::DidCloseTextDocumentParams),
}

impl json_rpc::Method for ClientNotification {
    fn method(&self) -> String {
        match *self {
            Self::Initialized => "initialized",
            Self::Exit => "exit",
            Self::OpenDoc(_) => "textDocument/didOpen",
            Self::CloseDoc(_) => "textDocument/didClose",
        }
        .to_string()
    }

    fn params(&self) -> json_rpc::Params {
        match *self {
            Self::Initialized | Self::Exit => json_rpc::Params::None,
            Self::OpenDoc(ref params) => {
                #[allow(clippy::expect_used)]
                // DidOpenTextDocumentParams can be serialized to JSON.
                json_rpc::Params::from(
                    serde_json::to_value(params)
                        .expect("Converting DidOpenTextDocumentParams to JSON Value"),
                )
            }
            Self::CloseDoc(ref params) => {
                #[allow(clippy::expect_used)]
                // DidCloseTextDocumentParams can be serialized to JSON.
                json_rpc::Params::from(
                    serde_json::to_value(params)
                        .expect("Converting DidCloseTextDocumentParams to JSON Value"),
                )
            }
        }
    }
}

/// A response from the client.
#[derive(Clone, Copy, Debug, parse_display::Display, PartialEq)]
pub enum ClientResponse {
    /// Confirms client registered capabilities.
    RegisterCapability,
}

impl json_rpc::Success for ClientResponse {
    fn result(&self) -> serde_json::Value {
        match *self {
            Self::RegisterCapability => serde_json::Value::Null,
        }
    }
}

/// Failed to create `Client`.
#[derive(Debug, thiserror::Error)]
pub enum CreateClientError {
    /// Failed to create server process.
    #[error(transparent)]
    CreateProcess(#[from] market::process::CreateProcessError),
}

/// Client failed to consume `lsp::ServerMessage`.
#[derive(ConsumeFault, Debug, thiserror::Error)]
pub enum ConsumeServerMessageError {
    /// Client failed to consume lsp::Message from server.
    #[error(transparent)]
    Consume(#[from] ReadFault<Message>),
    /// Client failed to convert lsp::Message into lsp::ServerMessage.
    #[error(transparent)]
    UnknownServerMessage(#[from] lsp::UnknownServerMessageFailure),
}
