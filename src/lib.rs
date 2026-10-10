//! foobar language tooling: one grammar, one core, three tools.
//!
//! - Syntax (every tool): [`syntax`], [`text`], [`format`].
//! - Semantic core (language server and linter): [`symbol`], [`workspace`], [`diagnostics`].
//! - Editor: [`document`], [`hover`], [`definition`], [`completion`], [`references`], [`rename`],
//!   [`keywords`], [`server`].
//! - Command line: [`cli`].

pub mod cli;
pub mod completion;
pub mod definition;
pub mod diagnostics;
pub mod document;
pub mod format;
pub mod hover;
pub mod keywords;
pub mod references;
pub mod rename;
pub mod server;
pub mod symbol;
pub mod syntax;
pub mod text;
pub mod workspace;
