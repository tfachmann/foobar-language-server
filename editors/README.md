# Editor setup

> [!WARNING]
> The code in this folder was written by a coding agent and has had little human review. Treat it
> as a starting point; the "Tested here" column below shows how far each setup was checked.

Every editor needs two things:

1. **The language server.** `cargo install --path .` in this repository puts `foobar` on your
   PATH; editors start it as `foobar lsp`. It provides diagnostics, hover, goto definition,
   references, rename, completion, formatting and folding.
2. **Syntax highlighting.** Editors with tree-sitter use the grammar directly: clone
   [tree-sitter-foobar](https://github.com/tfachmann/tree-sitter-foobar) and run
   `tree-sitter build -o parser.so` in it (Helix and Zed fetch it themselves). The others need
   their own format.

| Editor | Files | Highlighting | Language server | Tested here |
|---|---|---|---|---|
| Neovim 0.11+ | [`neovim/foobar.lua`](neovim/foobar.lua) | tree-sitter + `highlights.scm` | built-in `vim.lsp` | yes, headless: captures, diagnostics, hover |
| Helix | [`helix/languages.toml`](helix/languages.toml) | tree-sitter + `highlights.scm` | built-in | no, Helix not installed |
| Emacs 29+ | [`emacs/foobar-ts-mode.el`](emacs/foobar-ts-mode.el) | tree-sitter, rules restated in Elisp | eglot | yes, batch: faces, eglot diagnostics |
| Kate | [`kate/`](kate) | KSyntaxHighlighting XML | LSP Client plugin | no, files only checked for syntax |
| Zed | [`zed/`](zed) (extension) | tree-sitter + `highlights.scm` (bundled copy) | the extension starts `foobar lsp` | builds to Wasm; not run in Zed |
| VS Code | [`vscode/`](vscode) (extension) | TextMate grammar | vscode-languageclient | packaged, grammar tokenized; not run in VS Code |

Two camps: Neovim, Helix and Zed read the grammar's `highlights.scm` as is. Emacs uses the
grammar but needs the rules in Elisp; Kate and VS Code don't use tree-sitter at all, so their
highlighting restates the query in another format. Semantic tokens from the server would give those
editors highlighting from `highlights.scm` too (not implemented yet).

## Neovim

No plugins needed. Copy `neovim/foobar.lua` to `~/.config/nvim/plugin/` and set `grammar` (or
`$FOOBAR_GRAMMAR`) to the `tree-sitter-foobar` checkout. To try it without touching your config:

```sh
FOOBAR_GRAMMAR=/path/to/tree-sitter-foobar nvim --clean -u editors/neovim/foobar.lua file.foobar
```

With nvim-treesitter, register the parser as described in the `tree-sitter-foobar` README and keep
only the `vim.lsp.config` part.

## Helix

Append `helix/languages.toml` to `~/.config/helix/languages.toml`, run
`hx --grammar fetch && hx --grammar build` (fetches the grammar from GitHub at the pinned `rev`),
then link the fetched queries:
`ln -s ~/.config/helix/runtime/grammars/sources/foobar/queries ~/.config/helix/runtime/queries/foobar`.
Helix maps capture names to its own theme scopes; `@property` has no Helix scope and stays plain.

## Emacs

Emacs 29 loads parsers up to ABI 14, while the tree-sitter CLI generates ABI 15. Build one for
Emacs without touching the committed parser, then load the mode:

```sh
git clone https://github.com/tfachmann/tree-sitter-foobar && cd tree-sitter-foobar
tree-sitter generate --abi 14 -o /tmp/foobar-abi14/src
tree-sitter build -o ~/.emacs.d/tree-sitter/libtree-sitter-foobar.so /tmp/foobar-abi14
```

```elisp
(load "/path/to/foobar-language-server/editors/emacs/foobar-ts-mode.el")
(add-hook 'foobar-ts-mode-hook #'eglot-ensure)
```

## Kate

- Highlighting: copy `kate/foobar.xml` to `~/.local/share/org.kde.syntax-highlighting/syntax/`.
- Language server: enable the LSP Client plugin, then paste `kate/lspclient-settings.json` into
  Settings → Configure Kate → LSP Client → User Server Settings. `highlightingModeRegex` matches the
  language name from `foobar.xml`, so both files are needed.

## Zed

Run `zed: install dev extension` and pick `editors/zed`. Zed compiles the Rust part to Wasm (it
installs the `wasm32-wasip2` target through rustup) and fetches the grammar from GitHub at the
`rev` in `extension.toml`. The first install also downloads wasi-sdk (about 190 MB, once) to compile
the grammar, so "Installing foobar extension..." can spin for a minute or two; `zed: open log` shows
the progress, and a broken download is retried on the next install.

After pushing grammar changes, update `rev` and copy `tree-sitter-foobar/queries/highlights.scm` to
`zed/languages/foobar/`; a test in this repository fails when that copy is out of date.

## VS Code

See [`vscode/README.md`](vscode/README.md): `npm install`, `npm run build`, `npm run package`, then
`code --install-extension foobar-0.1.0.vsix`. Packaging uses `vsce` 3, which needs Node 20 or newer
(with bun: `bun install`, `bun run build`, `bun --bun node_modules/@vscode/vsce/vsce package
--no-dependencies --skip-license`).
