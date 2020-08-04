mod json_rpc;

use {
    core::{
        cell::{RefCell, Cell},
        convert::{TryFrom, TryInto},
    },
    enum_map::{enum_map, Enum},
    fehler::{throw, throws},
    json_rpc::{Success, Kind, Object, Outcome, Id, Params, Method},
    log::{error, trace},
    lsp_types::{Registration, PublishDiagnosticsParams, TextDocumentIdentifier, TextDocumentItem, RegistrationParams, notification::{PublishDiagnostics, Notification}, request::{Request, RegisterCapability}, InitializeParams, ClientCapabilities, InitializeResult, TextDocumentClientCapabilities, SynchronizationCapability, Url, DidOpenTextDocumentParams, DidCloseTextDocumentParams},
    market::{io::Reader, ConsumeFailure, ClosedMarketError, Consumer, ComposeFrom, NonComposible, StripFrom, PermanentQueue, Producer, ProduceFailure, process::{CreateProcessError, WaitProcessError, Waiter, Process}, sync::{Releaser, Actuator}},
    parse_display::Display as ParseDisplay,
    serde_json::{Number, Value, error::Error as SerdeJsonError},
    std::{fmt::{self, Display}, sync::Arc, rc::Rc, thread::{self, JoinHandle}, process::{self, Command}},
    thiserror::Error as ThisError,
};

/// The header field name that maps to the length of the content.
static HEADER_CONTENT_LENGTH: &str = "Content-Length";
/// The end of the header.
static HEADER_END: &str = "\r\n\r\n";

/// The languages supported by docuglot.
#[derive(Clone, Copy, Debug, Enum, ParseDisplay, PartialEq)]
#[display(style = "lowercase")]
pub enum Language {
    Rust,
    Plaintext,
}

#[derive(Debug)]
pub struct Tongue {
    thread: Option<JoinHandle<()>>,
    join_actuator: Actuator,
    outputs: Arc<PermanentQueue<ClientStatement>>,
    inputs: Arc<PermanentQueue<ServerStatement>>,
}

impl Tongue {
    pub fn new(root_dir: &Url) -> Self {
        let (join_actuator, releaser) = market::sync::trigger();
        let dir = root_dir.clone();
        let outputs = Arc::new(PermanentQueue::new());
        let shared_outputs = Arc::clone(&outputs);
        let inputs = Arc::new(PermanentQueue::new());
        let shared_inputs = Arc::clone(&inputs);

        Self {
            join_actuator,
            thread: Some(thread::spawn(move || {
                if let Err(error) = Tongue::thread(&dir, releaser, shared_outputs, shared_inputs) {
                    error!("tongue thread error: {}", error);
                }
            })),
            outputs,
            inputs,
        }
    }

    #[throws(TranslationError)]
    fn thread(root_dir: &Url, releaser: Releaser, outputs: Arc<PermanentQueue<ClientStatement>>, inputs: Arc<PermanentQueue<ServerStatement>>) {
        let rust_translator = Rc::new(RefCell::new(Translator::new(Client::new(
                Command::new("rust-analyzer"),
                )?, root_dir.clone())));
        // TODO: Currently plaintext_translator is a hack to deal with all files that do not have a known language. Ideally, this would run its own language server.
        let plaintext_translator = Rc::new(RefCell::new(Translator::new(Client::new(
                Command::new("echo"),
                )?, root_dir.clone())));
        plaintext_translator.borrow_mut().state = State::WaitingExit;
        let translators = enum_map! {
            Language::Rust => Rc::clone(&rust_translator),
            Language::Plaintext => Rc::clone(&plaintext_translator),
        };

        while !translators.values().map(|t| t.borrow().state == State::WaitingExit).all(|x| x) {
            let will_shutdown = releaser.consume().is_ok();

            for output in outputs.consume_all().unwrap() {
                translators[output.language()].borrow_mut().send_message(output.into())?;
            }

            for (_, translator) in &translators {
                if let Some(input) = translator.borrow_mut().translate()? {
                    inputs.produce(input).unwrap();
                }

                translator.borrow().log_errors();

                if will_shutdown {
                    translator.borrow_mut().shutdown()?;
                }
            }
        }

        for (_, translator) in translators {
            translator.borrow().waiter().demand()?;
        }
    }

    pub fn join(&mut self) {
        if let Some(thread) = self.thread.take() {
            self.join_actuator.produce(()).unwrap();

            thread.join().unwrap();
        }
    }
}

impl Consumer for Tongue {
    type Good = ServerStatement;
    type Error = <PermanentQueue<ServerStatement> as Consumer>::Error;

    #[throws(ConsumeFailure<Self::Error>)]
    fn consume(&self) -> Self::Good {
        self.inputs.consume()?
    }
}

impl Producer for Tongue {
    type Good = ClientStatement;
    type Error = <PermanentQueue<ClientStatement> as Producer>::Error;

    #[throws(ProduceFailure<Self::Error>)]
    fn produce(&self, good: Self::Good) {
        self.outputs.produce(good)?
    }
}

#[derive(Debug, ParseDisplay)]
#[display(style = "CamelCase")]
pub enum ServerStatement {
    Exit,
}

#[derive(Debug, ParseDisplay)]
#[display("")]
pub enum ClientStatement {
    OpenDoc {
        doc: TextDocumentItem,
    },
    CloseDoc {
        doc: TextDocumentIdentifier,
    },
}

impl ClientStatement {
    pub fn open_doc(doc: TextDocumentItem) -> Self {
        Self::OpenDoc{doc}
    }

    pub fn close_doc(doc: TextDocumentIdentifier) -> Self {
        Self::CloseDoc{doc}
    }

    fn language(&self) -> Language {
        Language::Rust
    }
}

impl From<ClientStatement> for ClientMessage {
    fn from(value: ClientStatement) -> Self {
        match value {
            ClientStatement::OpenDoc { doc } => Self::Notification(ClientNotification::OpenDoc(DidOpenTextDocumentParams{text_document: doc})),
            ClientStatement::CloseDoc { doc } => Self::Notification(ClientNotification::CloseDoc(DidCloseTextDocumentParams{text_document: doc})),
        }
    }
}

#[derive(Debug)]
pub struct ErrorMessage {
    line: String,
}

impl ComposeFrom<u8> for ErrorMessage {
    #[throws(NonComposible)]
    fn compose_from(parts: &mut Vec<u8>) -> Self {
        if let Ok(s) = std::str::from_utf8_mut(parts) {
            if let Some(index) = s.find('\n') {
                let (l, remainder) = s.split_at_mut(index);
                let (_, new_parts) = remainder.split_at_mut(1);
                let line = l.to_string();
                *parts = new_parts.as_bytes().to_vec();

                ErrorMessage {line}
            } else {
                // parts does not contain a new line.
                throw!(NonComposible);
            }
        } else {
            // parts has some invalid uft8.
            *parts = Vec::new();
            throw!(NonComposible);
        }
    }
}

impl Display for ErrorMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.line)
    }
}

#[derive(Debug, ThisError)]
pub enum TranslationError {
    #[error(transparent)]
    Transmission(#[from] ProduceFailure<ProduceClientMessageFailure>),
    #[error(transparent)]
    Reception(#[from] ConsumeServerMessageFailure),
    #[error(transparent)]
    CreateClient(#[from] CreateClientError),
    #[error(transparent)]
    Wait(#[from] WaitProcessError),
    #[error(transparent)]
    CollectOutputs(#[from] ConsumeFailure<<PermanentQueue<ClientMessage> as Consumer>::Error>),
    #[error("Invalid state: Cannot {0} while {1}")]
    InvalidState(Event, State),
}

#[derive(Debug, ParseDisplay, PartialEq)]
pub enum Event {
    #[display("send message")]
    SendMessage(ClientMessage),
    #[display("process initialization")]
    Initialized(InitializeResult),
    #[display("register capability")]
    RegisterCapability(Id, RegistrationParams),
    #[display("complete shutdown")]
    CompletedShutdown,
    #[display("exit")]
    Exit,
}

#[derive(Clone, Debug, ParseDisplay, PartialEq)]
pub enum State {
    #[display("uninitialized")]
    Uninitialized{
        root_dir: Url,
    },
    #[display("waiting initialization")]
    WaitingInitialization{
        messages: Vec<ClientMessage>,
    },
    #[display("running")]
    Running {
        server_state: InitializeResult,
        registrations: Vec<Registration>,
    },
    #[display("waiting shutdown")]
    WaitingShutdown,
    #[display("waiting exit")]
    WaitingExit,
}

pub(crate) struct Translator {
    client: Client,
    state: State,
}

impl Translator {
    pub fn new(client: Client, root_dir: Url) -> Self {
        Self{client, state: State::Uninitialized{root_dir}}
    }

    #[throws(TranslationError)]
    pub fn send_message(&mut self, message: ClientMessage) {
        self.process(Event::SendMessage(message))?
    }

    #[throws(TranslationError)]
    pub fn translate(&mut self) -> Option<ServerStatement> {
        let mut statement = None;

        match self.client.consume() {
            Ok(message) => {
                match message {
                    ServerMessage::Request { id, request } => {
                        match request {
                            ServerRequest::RegisterCapability(registration) => {
                                self.process(Event::RegisterCapability(id, registration))?;
                            }
                        }
                    }
                    ServerMessage::Response(response) => match response {
                        ServerResponse::Initialize(initialize) => {
                            self.process(Event::Initialized(initialize))?;
                        }
                        ServerResponse::Shutdown => {
                            self.process(Event::CompletedShutdown)?;
                        }
                    }
                    ServerMessage::Notification(notification) => match notification {
                        ServerNotification::PublishDiagnostics(diagnostics) => {
                            // TODO: Send diagnostics to tool.
                        }
                    }
                }
            }
            Err(error) => {
                if let ConsumeFailure::Error(failure) = error {
                    throw!(TranslationError::from(failure));
                }
            }
        }

        statement
    }

    #[throws(TranslationError)]
    fn process(&mut self, event: Event) {
        match &self.state {
            State::Uninitialized{root_dir} => match event {
                Event::SendMessage(message) => {
                    #[allow(deprecated)] // InitializeParams.root_path is required.
                    self.client.produce(ClientMessage::Request(ClientRequest::Initialize(
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
                    self.state = State::WaitingInitialization { messages: vec![message] }
                }
                Event::Initialized(_) | Event::CompletedShutdown | Event::RegisterCapability(..) => {
                    throw!(TranslationError::InvalidState(event, self.state.clone()));
                }
                Event::Exit => {
                    self.client.produce(ClientMessage::Notification(ClientNotification::Exit))?;
                    self.state = State::WaitingExit
                }
            }
            State::WaitingInitialization{messages} => match event {
                Event::SendMessage(message) => {
                    let mut new_messages = messages.clone();
                    new_messages.push(message);
                    self.state = State::WaitingInitialization{messages: new_messages}
                }
                Event::Initialized(server_state) => {
                    self.client.produce(ClientMessage::Notification(ClientNotification::Initialized))?;

                    for message in messages {
                        self.client.produce(message.clone())?;
                    }

                    self.state = State::Running{server_state, registrations: Vec::new()}
                }
                Event::CompletedShutdown | Event::RegisterCapability(..) => {
                    throw!(TranslationError::InvalidState(event, self.state.clone()));
                }
                Event::Exit => {
                    // TODO: Figure out how to handle this case.
                }
            }
            State::Running{server_state, registrations} => match event {
                Event::SendMessage(message) => {
                    self.client.produce(message)?;
                }
                Event::RegisterCapability(id, register) => {
                    let mut new_registrations = registrations.clone();
                    new_registrations.append(&mut register.registrations.clone());
                    self.state = State::Running{server_state: server_state.clone(), registrations: new_registrations};
                    self.client.produce(ClientMessage::Response{
                        id,
                        response: ClientResponse::RegisterCapability
                    })?;
                }
                Event::Initialized(_) | Event::CompletedShutdown => {
                    throw!(TranslationError::InvalidState(event, self.state.clone()));
                }
                Event::Exit => {
                    self.client.produce(ClientMessage::Request(ClientRequest::Shutdown))?;
                    self.state = State::WaitingShutdown;
                }
            }
            State::WaitingShutdown => match event {
                Event::SendMessage(_) | Event::Initialized(_) | Event::Exit | Event::RegisterCapability(..) => {
                    throw!(TranslationError::InvalidState(event, self.state.clone()));
                }
                Event::CompletedShutdown => {
                    self.client.produce(ClientMessage::Notification(ClientNotification::Exit))?;
                    self.state = State::WaitingExit;
                }
            }
            State::WaitingExit => {
                if event != Event::Exit {
                    throw!(TranslationError::InvalidState(event, self.state.clone()));
                }
            }
        }
    }

    pub fn log_errors(&self) {
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

    #[throws(TranslationError)]
    pub fn shutdown(&mut self) {
        self.process(Event::Exit)?;
    }

    pub fn waiter(&self) -> &Waiter {
        self.client.waiter()
    }
}

pub(crate) struct Client {
    server: Process<Message, Message, ErrorMessage>,
    next_id: Cell<u64>,
}

impl Client {
    /// Creates a new [`Client`] for `language`.
    #[throws(CreateClientError)]
    pub fn new(command: Command) -> Self {
        Self {
            server: Process::new(command)?,
            next_id: Cell::new(1),
        }
    }

    pub fn waiter(&self) -> &Waiter {
        self.server.waiter()
    }

    pub fn stderr(&self) -> &Reader<ErrorMessage> {
        self.server.stderr()
    }

    fn next_id(&self) -> Id {
        let id = self.next_id.get();
        self.next_id.set(id.wrapping_add(1));
        Id::Num(Number::from(id))
    }
}

impl Consumer for Client {
    type Good = ServerMessage;
    type Error = ConsumeServerMessageFailure;

    #[throws(ConsumeFailure<Self::Error>)]
    fn consume(&self) -> Self::Good {
        let good = self.server.consume().map_err(ConsumeFailure::map_into)?.try_into().map_err(|error| ConsumeFailure::Error(Self::Error::from(error)))?;
        trace!("LSP Rx: {}", good);
        good
    }
}

impl Producer for Client {
    type Good = ClientMessage;
    type Error = ProduceClientMessageFailure;

    #[throws(ProduceFailure<Self::Error>)]
    fn produce(&self, good: Self::Good) {
        trace!("LSP Tx: {}", good);
        let message = Message::from(match good {
            ClientMessage::Response { id, response } => Object::response(id, response),
            ClientMessage::Request(request) =>  Object::request(self.next_id(), request),
            ClientMessage::Notification(notification) => Object::notification(notification),
        });

        self.server.produce(message).map_err(ProduceFailure::map_into)?
    }
}

/// A message from the language server.
#[derive(Debug, ParseDisplay)]
pub(crate) enum ServerMessage {
    #[display("Response: {0}")]
    Response(ServerResponse),
    #[display("Request[{id:?}]: {request}")]
    Request {
        id: Id,
        request: ServerRequest,
    },
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
            } => Self::Request { id : request_id , request: ServerRequest::new(method, params)?},
            Kind::Request {
                id: None,
                method,
                params,
            } => {
                Self::Notification(ServerNotification::new(method, params)?)
            }
            Kind::Response {
                outcome: Outcome::Result(value),
                ..
            } => {
                Self::Response(value.try_into()?)
            }
        }
    }
}

#[derive(Debug, ParseDisplay)]
pub enum ServerNotification {
    #[display("PublishDiagnostics({0:?})")]
    PublishDiagnostics(PublishDiagnosticsParams),
}

impl ServerNotification {
    #[throws(UnknownServerMessageFailure)]
    fn new(method: String, params: Params) -> Self {
        match method.as_str() {
            <PublishDiagnostics as Notification>::METHOD => Self::PublishDiagnostics(serde_json::from_value::<PublishDiagnosticsParams>(params.clone().into()).map_err(|error| UnknownServerMessageFailure::InvalidParams{method, params, error})?),
            _ => throw!(UnknownServerMessageFailure::UnknownMethod(method)),
        }
    }
}

#[derive(Debug, ParseDisplay)]
pub enum ServerRequest {
    #[display("RegisterCapability({0:?})")]
    RegisterCapability(RegistrationParams),
}

impl ServerRequest {
    #[throws(UnknownServerMessageFailure)]
    fn new(method: String, params: Params) -> Self {
        match method.as_str() {
            <RegisterCapability as Request>::METHOD => ServerRequest::RegisterCapability(serde_json::from_value::<RegistrationParams>(params.clone().into()).map_err(|error| UnknownServerMessageFailure::InvalidParams{method, params, error})?),
            _ => throw!(UnknownServerMessageFailure::UnknownMethod(method)),
        }
    }
}

#[derive(Debug, ParseDisplay)]
pub enum ServerResponse {
    #[display("{0:?}")]
    Initialize(InitializeResult),
    Shutdown,
}

impl TryFrom<Value> for ServerResponse {
    type Error = UnknownServerResponseFailure;

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

/// A message to the language server.
#[derive(Clone, Debug, PartialEq)]
pub enum ClientMessage {
    Request(ClientRequest),
    Response {
        id: Id,
        response: ClientResponse,
    },
    Notification(ClientNotification),
}

impl Display for ClientMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", match self {
            Self::Request(request) => format!("{}: {}", "Request", request),
            Self::Response{response, ..} => format!("{}: {}", "Response", response),
            Self::Notification(notification) => format!("{}: {}", "Notification", notification),
        })
    }
}

#[derive(Clone, Debug, ParseDisplay, PartialEq)]
pub enum ClientRequest {
    #[display("Initialize w/ {0:?}")]
    Initialize(InitializeParams),
    Shutdown,
}

impl Method for ClientRequest {
    fn method(&self) -> String {
        match self {
            Self::Initialize(_) => "initialize",
            Self::Shutdown => "shutdown",
        }.to_string()
    }

    fn params(&self) -> Params {
        match self {
            Self::Initialize(params) => Params::from(serde_json::to_value(params).unwrap()),
            Self::Shutdown => Params::None,
        }
    }
}

#[derive(Clone, Debug, ParseDisplay, PartialEq)]
pub enum ClientNotification {
    Initialized,
    Exit,
    #[display("OpenDoc w/ {0:?}")]
    OpenDoc(DidOpenTextDocumentParams),
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
        }.to_string()
    }

    fn params(&self) -> Params {
        match self {
            Self::Initialized => Params::None,
            Self::Exit => Params::None,
            Self::OpenDoc(params) => Params::from(serde_json::to_value(params).unwrap()),
            Self::CloseDoc(params) => Params::from(serde_json::to_value(params).unwrap()),
        }
    }
}

#[derive(Clone, Debug, ParseDisplay, PartialEq)]
pub enum ClientResponse {
    RegisterCapability,
}

impl Success for ClientResponse {
    fn result(&self) -> Value {
        match self {
            Self::RegisterCapability => Value::Null,
        }
    }
}

#[derive(Debug, ParseDisplay)]
#[display("{content}")]
pub(crate) struct Message {
    /// The JSON-RPC object of the message.
    content: Object,
}

//impl Message {
//    /// Creates a new [`Message`].
//    fn new(object: Object) -> Self {
//        Self {
//            object,
//        }
//    }
//
//    pub fn object(&self) -> &Object {
//        &self.object
//    }
//
//    /// Creates an LSP Request Message.
//    #[throws(SerdeJsonError)]
//    pub fn request<T>(params: T::Params, id: Id) -> Self
//    where
//        T: Request,
//        <T as Request>::Params: Serialize,
//    {
//        Object::request::<T>(id, params).map(Self::new)?
//    }
//
//    /// Creates an LSP Notification Message.
//    #[throws(SerdeJsonError)]
//    pub fn notification<T>(params: T::Params) -> Self
//    where
//        T: Notification,
//        <T as Notification>::Params: Serialize,
//    {
//        Object::notification::<T>(params).map(Self::new)?
//    }
//
//    /// Creates an LSP Response Message.
//    #[throws(SerdeJsonError)]
//    pub fn response<T>(result: T::Result, id: Id) -> Self
//    where
//        T: Request,
//        <T as Request>::Result: Serialize,
//    {
//        Object::response::<T>(result, id).map(Self::new)?
//    }
//}

impl ComposeFrom<u8> for Message {
    #[throws(NonComposible)]
    fn compose_from(parts: &mut Vec<u8>) -> Self {
        let mut length = 0;

        let object: Option<Object> = std::str::from_utf8(parts).ok().and_then(|buffer| {
            buffer.find(HEADER_END).and_then(|header_length| {
                let mut content_length: Option<usize> = None;

                buffer.get(..header_length).and_then(|header| {
                    let content_start = header_length.saturating_add(HEADER_END.len());

                    for field in header.split("\r\n") {
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

                    match content_length {
                        None => {
                            length = header_length;
                            None
                        }
                        Some(content_length) => {
                            if let Some(total_len) = content_start.checked_add(content_length) {
                                if parts.len() < total_len {
                                    None
                                } else if let Some(content) = buffer.get(content_start..total_len) {
                                    length = total_len;
                                    serde_json::from_str(content).ok()
                                } else {
                                    length = content_start;
                                    None
                                }
                            } else {
                                length = content_start;
                                None
                            }
                        }
                    }
                })
            })
        });

        let _ = parts.drain(..length);
        object.ok_or(NonComposible)?.into()
    }
}

impl From<Object> for Message {
    fn from(value: Object) -> Self {
        Self {
            content: value,
        }
    }
}

impl StripFrom<Message> for u8 {
    fn strip_from(good: &Message) -> Vec<Self> {
        serde_json::to_string(&good.content).map_or(Vec::new(), |content| {
            format!(
                "{}: {}{}{}",
                HEADER_CONTENT_LENGTH,
                content.len(),
                HEADER_END,
                content
            )
            .as_bytes()
            .to_vec()
        })
    }
}

#[derive(Debug, ThisError)]
pub enum CreateClientError {
    #[error(transparent)]
    CreateProcess(#[from] CreateProcessError),
    #[error(transparent)]
    Initialize(#[from] ProduceFailure<ProduceClientMessageFailure>),
}

#[derive(Debug, ThisError)]
pub enum UnknownServerMessageFailure {
    #[error(transparent)]
    Response(#[from] UnknownServerResponseFailure),
    #[error("Unknown method: {0}")]
    UnknownMethod(String),
    #[error("Unable to convert `{method}` from `{params}: {error}`")]
    InvalidParams{
        method: String,
        params: Params,
        #[source]
        error: SerdeJsonError,
    },
}

#[derive(Debug, ThisError)]
#[error("Unknown response from server")]
pub struct UnknownServerResponseFailure;

#[derive(Debug, ThisError)]
pub enum ConsumeServerMessageFailure {
    #[error(transparent)]
    ClosedServer(#[from] ClosedMarketError),
    #[error(transparent)]
    UnknownServerMessage(#[from] UnknownServerMessageFailure),
}

#[derive(Debug, ThisError)]
pub enum ProduceClientMessageFailure {
    #[error(transparent)]
    Serialize(#[from] SerdeJsonError),
    #[error(transparent)]
    ClosedServer(#[from] ClosedMarketError),
}
