//! An interface with any number of Language Servers.
#![allow(clippy::pattern_type_mismatch)] // Marks enums as errors.
mod json_rpc;
mod lsp;

use {
    core::{
        cell::RefCell,
    },
    fehler::{throw, throws},
    lsp::{TranslationError, Tool, Event, ClientRequest, ClientMessage, ClientNotification},
    lsp_types::{Url, DocumentSymbolResponse, TextDocumentIdentifier, DidOpenTextDocumentParams, DidCloseTextDocumentParams, DocumentSymbolParams, WorkDoneProgressParams, PartialResultParams},
    market::{
        channel::{
            create, Crossbeam, CrossbeamConsumer, CrossbeamProducer, Size, Structure,
        },
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

struct TongueThreadParams {
    root_dir: Url,
    transmission_consumer: CrossbeamConsumer<Transmission>,
    reception_producer: CrossbeamProducer<Reception>,
    status_producer: CrossbeamProducer<ExitStatus>,
}

/// An interface to all Language Servers.
///
/// SHALL execute the following:
/// - handle the initialization of the appropriate langugage server(s) when they are needed.
/// - provide access to produce [`Transmission`]s by converting them into messages and sending them to the appropriate language server.
///
/// Following the precedent set by [`Process`], [`Tongues`] shall impl [`Consumer`] of its status, while providing references to the input and output actors.
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
    pub fn new(root_dir: &Url) -> Self {
        let (transmitter, transmission_consumer) =
            create::<Crossbeam<Transmission>>(Structure::BilateralMonopoly, Size::Infinite);
        let (reception_producer, receiver) =
            create::<Crossbeam<Reception>>(Structure::BilateralMonopoly, Size::Infinite);
        let (status_producer, status_consumer) =
            create::<Crossbeam<ExitStatus>>(Structure::BilateralMonopoly, Size::Infinite);
        let params = TongueThreadParams {
            root_dir: root_dir.clone(),
            transmission_consumer,
            reception_producer,
            status_producer,
        };

        Self {
            thread: Thread::new(thread::Kind::Single, params, Self::thread),
            status_consumer,
            transmitter,
            receiver,
        }
    }

    /// The main thread of a Tongue.
    #[throws(TranslationError)]
    fn thread(params: &mut TongueThreadParams) {
        // TODO: Currently default_translator is a hack to deal with all files that do not have a known language. Ideally, this would run its own language server.
        let default_translator = Rc::new(RefCell::new(Tool::new(
            Command::new("echo"),
            params.root_dir.clone(),
        )?));
        // Currently must use a hack in order to store non-Copy Tool as value in enum_map. See https://gitlab.com/KonradBorowski/enum-map/-/issues/15.
        let rust_translator = Rc::new(RefCell::new(Tool::new_finished(
            Command::new("rust-analyzer"),
        )?));
        let translators = [
            Rc::clone(&rust_translator),
            Rc::clone(&default_translator),
        ];

        while !translators
            .iter()
            .map(|t| t.borrow().is_waiting_exit())
            .all(|x| x)
        {
            for transmission in params.transmission_consumer.consume_iter()? {
                if let Some(language) = transmission.language() {
                    match language {
                        Language::Rust => &rust_translator,
                    }
                } else {
                    &default_translator
                }.borrow_mut().transmit(vec![transmission.into()])?;
            }

            for translator in translators.iter() {
                let mut receptions = Vec::new();

                for event in translator.borrow_mut().process_receptions()? {
                    match event {
                        Event::SendMessages(messages) => translator.borrow_mut().transmit(messages)?,
                        Event::Error(error) => throw!(error),
                        Event::DocumentSymbol(document_symbol) => {
                            receptions.push(Reception::DocumentSymbols(document_symbol));
                        }
                    }
                }

                params.reception_producer.produce_all(receptions)?;
                translator.borrow().log_errors();
            }
        }

        for translator in translators.iter() {
            params.status_producer.produce(translator.borrow().server().demand()?)?;
        }
    }

    /// Returns a reference to the [`Transmission`] [`Producer`].
    pub fn transmitter(&self) -> &CrossbeamProducer<Transmission> {
        &self.transmitter
    }

    /// Returns a reference to the [`Reception`] [`Consumer`].
    pub fn receiver(&self) -> &CrossbeamConsumer<Reception> {
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
            Transmission::OpenDoc { doc } => ClientNotification::OpenDoc(DidOpenTextDocumentParams { text_document: doc }).into(),
            Transmission::CloseDoc { doc } => ClientNotification::CloseDoc(DidCloseTextDocumentParams { text_document: doc}).into(),
            Transmission::GetDocumentSymbol { doc } => ClientRequest::DocumentSymbol(DocumentSymbolParams { text_document: doc, work_done_progress_params: WorkDoneProgressParams::default(), partial_result_params: PartialResultParams::default() }).into(),
            Transmission::Shutdown => ClientMessage::Shutdown,
        }
    }
}

/// A communication received from a language server.
#[derive(Debug, parse_display::Display)]
#[display(style = "CamelCase")]
pub enum Reception {
    #[display("{0:?}")]
    // TODO: Should include the document that symbols come from.
    DocumentSymbols(DocumentSymbolResponse),
}
