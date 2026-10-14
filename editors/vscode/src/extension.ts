// Starts `foobar lsp` for foobar files. Highlighting comes from syntaxes/foobar.tmLanguage.json.

import { ExtensionContext, workspace } from "vscode";
import { LanguageClient, LanguageClientOptions, ServerOptions } from "vscode-languageclient/node";

let client: LanguageClient | undefined;

export async function activate(_context: ExtensionContext): Promise<void> {
  const command = workspace.getConfiguration("foobar").get<string>("server.path", "foobar");
  const serverOptions: ServerOptions = { command, args: ["lsp"] };
  const clientOptions: LanguageClientOptions = {
    documentSelector: [{ scheme: "file", language: "foobar" }],
  };
  client = new LanguageClient("foobar", "foobar language server", serverOptions, clientOptions);
  await client.start();
}

export async function deactivate(): Promise<void> {
  await client?.stop();
}
