//! Zed extension for foobar: highlighting comes from the bundled grammar and query, this code
//! only tells Zed how to start the language server.

use zed_extension_api::{self as zed, LanguageServerId, Result};

struct Foobar;

impl zed::Extension for Foobar {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        _language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        let command = worktree.which("foobar").ok_or_else(|| {
            "`foobar` is not on PATH: run `cargo install --path .` in foobar-language-server"
                .to_string()
        })?;
        Ok(zed::Command {
            command,
            args: vec!["lsp".to_string()],
            env: Vec::new(),
        })
    }
}

zed::register_extension!(Foobar);
