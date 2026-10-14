# foobar for VS Code

Syntax highlighting (a TextMate grammar) and the foobar language server: diagnostics, hover,
goto definition, references, rename, completion and formatting.

The extension starts `foobar lsp`, so build and install the server first
(`cargo install --path .` in foobar-language-server), or set `foobar.server.path`.

```sh
npm install          # or: bun install
npm run check        # type check
npm run build        # bundle to out/extension.js
npm run package      # foobar-0.1.0.vsix
code --install-extension foobar-0.1.0.vsix
```
