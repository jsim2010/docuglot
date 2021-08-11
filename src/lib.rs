//! An interface with any number of Language Servers.
mod json_rpc;
mod lsp;

pub use lsp::TransmissionError;

use {
    core::{
        cell::RefCell,
        fmt::{self, Display, Formatter},
    },
    fehler::{throw, throws},
    lsp::{CreateToolError, Directive, Event, InvalidStateError, ReceptionError, Tool},
    lsp_types::{
        DidCloseTextDocumentParams, DidOpenTextDocumentParams, DocumentSymbolParams,
        DocumentSymbolResponse, PartialResultParams, TextDocumentIdentifier, Url,
        WorkDoneProgressParams,
    },
    market::{
        channel::{InfiniteChannel, WithdrawnDemand, WithdrawnSupply},
        Consumer, Producer, Recall,
    },
    market_types::{
        channel_std::{StdInfiniteChannel, StdReceiver, StdSender},
        thread::Thread,
    },
    std::{
        process::{Command, ExitStatus},
        rc::Rc,
    },
};

/// The languages supported by [`Tongue`].
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum Language {
    /// Rust.
    Rust,
}

impl Display for Language {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Rust => write!(f, "rust"),
        }
    }
}

/// Params passed to the thread in [`Tongue`].
struct TongueThreadParams {
    /// The root directory.
    root_dir: Url,
    /// Consumes [`Transmission`]s.
    transmission_consumer: StdReceiver<Transmission>,
    /// Produces [`Reception`]s.
    reception_producer: StdSender<Reception>,
    /// Produces thread status.
    status_producer: StdSender<ExitStatus>,
}

/// Creates a [`Tongue`].
#[must_use]
pub fn create_tongue(root_dir: &Url) -> (StdSender<Transmission>, StdReceiver<Reception>, Tongue) {
    let (transmitter, transmission_consumer) =
        StdInfiniteChannel::<Transmission>::establish("Tongue Transmission Channel");
    let (reception_producer, receiver) =
        StdInfiniteChannel::<Reception>::establish("Tongue Reception Channel");
    let (status_producer, status_consumer) =
        StdInfiniteChannel::<ExitStatus>::establish("Tongue Status Channel");
    let params = TongueThreadParams {
        root_dir: root_dir.clone(),
        transmission_consumer,
        reception_producer,
        status_producer,
    };

    (
        transmitter,
        receiver,
        Tongue {
            thread: Thread::new(params, Tongue::thread_fn, "docuglot tongue"),
            status_consumer,
        },
    )
}

/// An error thrown by [`Tongue`].
#[derive(Debug, thiserror::Error)]
#[error("")]
#[non_exhaustive]
pub enum TongueError {
    /// An error when creating a client.
    CreateTool(#[from] CreateToolError),
    /// An error when transmitting.
    Transmit(#[from] TransmissionError),
    /// An error when receiving.
    Receive(#[from] ReceptionError),
    /// The language tool is in an invalid state.
    InvalidState(#[from] InvalidStateError),
    /// A [`WithdrawnSupply`].
    WithdrawnSupply(#[from] WithdrawnSupply),
    /// A [`Recall`].
    Recall(#[from] Recall<WithdrawnDemand, Reception>),
    /// A [`Recall`] of an [`ExitStatus`].
    Return(#[from] Recall<WithdrawnDemand, ExitStatus>),
}

/// An interface to all Language Servers.
///
/// SHALL execute the following:
/// - handle the initialization of the appropriate langugage server(s) when they are needed.
/// - provide access to produce [`Transmission`]s by converting them into messages and sending them to the appropriate language server.
///
/// "Tongue" refers to the ability of this item to be a tool that is used to communicate in multiple languages, just as a human tongue.
#[derive(Debug)]
pub struct Tongue {
    /// The thread.
    thread: Thread<(), TongueError>,
    /// The statuses of the Translators.
    status_consumer: StdReceiver<ExitStatus>,
}

impl Tongue {
    /// The main thread of a Tongue.
    #[throws(TongueError)]
    fn thread_fn(params: &mut TongueThreadParams) {
        let default_translator = Rc::new(RefCell::new(Tool::new_finished(Command::new("echo"))?));
        // Currently must use a hack in order to store non-Copy Tool as value in enum_map. See https://gitlab.com/KonradBorowski/enum-map/-/issues/15.
        let rust_translator = Rc::new(RefCell::new(Tool::new(
            Command::new("rust-analyzer"),
            params.root_dir.clone(),
        )?));
        let translators = [Rc::clone(&rust_translator), Rc::clone(&default_translator)];

        while !translators
            .iter()
            .map(|t| t.borrow().is_waiting_exit())
            .all(|x| x)
        {
            while let Ok(transmission) = params.transmission_consumer.consume() {
                transmission
                    .language()
                    .map_or(&default_translator, |language| match language {
                        Language::Rust => &rust_translator,
                    })
                    .borrow_mut()
                    .transmit(vec![transmission.into()])?;
            }

            for translator in &translators {
                let mut t = translator.borrow_mut();

                for event in t.process_receptions()? {
                    match event {
                        Event::SendDirectives(directives) => t.transmit(directives)?,
                        Event::Error(error) => throw!(error),
                        Event::DocumentSymbol(document_symbol) => {
                            params
                                .reception_producer
                                .produce(Reception::DocumentSymbols(document_symbol))?;
                        }
                        Event::Exit(status) => {
                            params.status_producer.produce(status)?;
                        }
                    }
                }
            }
        }
    }

    /// Returns a reference to the thead.
    #[must_use]
    pub const fn thread(&self) -> &Thread<(), TongueError> {
        &self.thread
    }
}

/// A communication to be sent to a language server.
#[derive(Debug)]
#[non_exhaustive]
pub enum Transmission {
    /// The tool opened `doc`.
    OpenDoc {
        /// The document that was opened.
        doc: lsp_types::TextDocumentItem,
    },
    /// The tool closed `doc`.
    CloseDoc {
        /// The document that was closed.
        doc: TextDocumentIdentifier,
    },
    /// The tool is requesting document symbols.
    GetDocumentSymbol {
        /// The document.
        doc: TextDocumentIdentifier,
    },
    /// The tool is shutting down the language server.
    Shutdown,
}

impl Transmission {
    /// Creates a `didClose` `Transmission`.
    #[must_use]
    pub const fn close_doc(doc: TextDocumentIdentifier) -> Self {
        Self::CloseDoc { doc }
    }

    /// Returns the language of `self`.
    #[allow(clippy::unused_self, clippy::unnecessary_wraps)] // Will require self and optionally return None in the future.
    const fn language(&self) -> Option<Language> {
        Some(Language::Rust)
    }
}

impl From<Transmission> for Directive {
    fn from(transmission: Transmission) -> Self {
        match transmission {
            Transmission::OpenDoc { doc } => {
                Self::OpenDoc(DidOpenTextDocumentParams { text_document: doc })
            }
            Transmission::CloseDoc { doc } => {
                Self::CloseDoc(DidCloseTextDocumentParams { text_document: doc })
            }
            Transmission::GetDocumentSymbol { doc } => Self::DocumentSymbol(DocumentSymbolParams {
                text_document: doc,
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            }),
            Transmission::Shutdown => Self::Shutdown,
        }
    }
}

/// A communication received from a language server.
#[derive(Debug)]
#[non_exhaustive]
pub enum Reception {
    /// The symbols in a document.
    DocumentSymbols(DocumentSymbolResponse),
}

impl Display for Reception {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        #[allow(clippy::use_debug)] // DocumentSymbolResponse does not impl Display.
        match *self {
            Self::DocumentSymbols(ref response) => write!(f, "{0:?}", response),
        }
    }
}
