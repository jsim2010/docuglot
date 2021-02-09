//! An interface with any number of Language Servers.
#![allow(clippy::pattern_type_mismatch)] // Marks enums as errors.
mod json_rpc;
mod lsp;

pub use lsp::TranslationError;

use {
    core::cell::RefCell,
    fehler::{throw, throws},
    lsp::{ClientMessage, Event, Tool},
    lsp_types::{
        DidCloseTextDocumentParams, DidOpenTextDocumentParams, DocumentSymbolParams,
        DocumentSymbolResponse, PartialResultParams, TextDocumentIdentifier, Url,
        WorkDoneProgressParams,
    },
    market::{
        channel::{create, Crossbeam, CrossbeamConsumer, CrossbeamProducer, Size},
        thread::{self, Thread},
        Consumer, Producer,
    },
    std::{
        process::{Command, ExitStatus},
        rc::Rc,
    },
};

/// The languages supported by [`Tongue`].
#[derive(Clone, Copy, Debug, parse_display::Display, PartialEq)]
#[display(style = "lowercase")]
#[non_exhaustive]
pub enum Language {
    /// Rust.
    Rust,
}

/// Params passed to the thread in [`Tongue`].
struct TongueThreadParams {
    /// The root directory.
    root_dir: Url,
    /// Consumes [`Transmission`]s.
    transmission_consumer: CrossbeamConsumer<Transmission>,
    /// Produces [`Reception`]s.
    reception_producer: CrossbeamProducer<Reception>,
    /// Produces thread status.
    status_producer: CrossbeamProducer<ExitStatus>,
}

/// An interface to all Language Servers.
///
/// SHALL execute the following:
/// - handle the initialization of the appropriate langugage server(s) when they are needed.
/// - provide access to produce [`Transmission`]s by converting them into messages and sending them to the appropriate language server.
///
/// Following the precedent set by [`market::process::Process`], [`Tongue`] shall impl [`Consumer`] of its status, while providing references to the input and output actors.
///
/// "Tongue" refers to the ability of this item to be a tool that is used to communicate in multiple languages, just as a human tongue.
// TODO: Implement a Consumer on the status of Tongue.
#[derive(Debug)]
pub struct Tongue {
    /// The thread.
    thread: Thread<(), TranslationError>,
    /// The statuses of the Translators.
    status_consumer: CrossbeamConsumer<ExitStatus>,
    /// The [`Transmission`] [`Producer`].
    transmitter: CrossbeamProducer<Transmission>,
    /// The [`Reception`] [`Consumer`].
    receiver: CrossbeamConsumer<Reception>,
}

impl Tongue {
    /// Creates a new [`Tongue`].
    #[inline]
    #[must_use]
    pub fn new(root_dir: &Url) -> Self {
        let (transmitter, transmission_consumer) = create::<Crossbeam<Transmission>>(
            "Tongue Transmission Channel".to_string(),
            Size::Infinite,
        );
        let (reception_producer, receiver) =
            create::<Crossbeam<Reception>>("Tongue Reception Channel".to_string(), Size::Infinite);
        let (status_producer, status_consumer) =
            create::<Crossbeam<ExitStatus>>("Tongue Status Channel".to_string(), Size::Infinite);
        let params = TongueThreadParams {
            root_dir: root_dir.clone(),
            transmission_consumer,
            reception_producer,
            status_producer,
        };

        Self {
            thread: Thread::new(
                "docuglot tongue".to_string(),
                thread::Kind::Single,
                params,
                Self::thread_fn,
            ),
            status_consumer,
            transmitter,
            receiver,
        }
    }

    /// The main thread of a Tongue.
    #[throws(TranslationError)]
    fn thread_fn(params: &mut TongueThreadParams) {
        // TODO: Currently default_translator is a hack to deal with all files that do not have a known language. Ideally, this would run its own language server.
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
            for good in params.transmission_consumer.goods() {
                let transmission = good?;
                transmission
                    .language()
                    .map_or(&default_translator, |language| match language {
                        Language::Rust => &rust_translator,
                    })
                    .borrow_mut()
                    .transmit(vec![transmission.into()])?;
            }

            for translator in &translators {
                let mut receptions = Vec::new();
                let mut t = translator.borrow_mut();

                for event in t.process_receptions()? {
                    match event {
                        Event::SendMessages(messages) => t.transmit(messages)?,
                        Event::Error(error) => throw!(error),
                        Event::DocumentSymbol(document_symbol) => {
                            receptions.push(Reception::DocumentSymbols(document_symbol));
                        }
                    }
                }

                params.reception_producer.produce_all(receptions)?;
                t.log_errors();
            }
        }

        for translator in &translators {
            params
                .status_producer
                .produce(translator.borrow().server().demand()?)?;
        }
    }

    /// Returns a reference to the thead.
    // TODO: Possibly should move this to Consumer impl of Tongue.
    #[inline]
    #[must_use]
    pub const fn thread(&self) -> &Thread<(), TranslationError> {
        &self.thread
    }

    /// Returns a reference to the [`Transmission`] [`Producer`].
    #[inline]
    #[must_use]
    pub const fn transmitter(&self) -> &CrossbeamProducer<Transmission> {
        &self.transmitter
    }

    /// Returns a reference to the [`Reception`] [`Consumer`].
    #[inline]
    #[must_use]
    pub const fn receiver(&self) -> &CrossbeamConsumer<Reception> {
        &self.receiver
    }
}

/// A communication to be sent to a language server.
#[derive(Debug, parse_display::Display)]
#[display("")]
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
    #[inline]
    #[must_use]
    pub const fn close_doc(doc: TextDocumentIdentifier) -> Self {
        Self::CloseDoc { doc }
    }

    /// Returns the language of `self`.
    #[allow(clippy::unused_self)] // Will require self in the future.
    const fn language(&self) -> Option<Language> {
        Some(Language::Rust)
    }
}

impl From<Transmission> for ClientMessage {
    #[inline]
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
#[derive(Debug, parse_display::Display)]
#[display(style = "CamelCase")]
pub enum Reception {
    /// The symbols in a document.
    #[display("{0:?}")]
    // TODO: Should include the document that symbols come from.
    DocumentSymbols(DocumentSymbolResponse),
}
