-- foobar for Neovim 0.11+, without plugins: highlighting, language server, comments.
--
-- Copy to ~/.config/nvim/plugin/foobar.lua (or source it from init.lua) and point `grammar` at a
-- built clone of https://github.com/tfachmann/tree-sitter-foobar (`tree-sitter build -o parser.so`).
-- Try it without touching your config: `nvim --clean -u editors/neovim/foobar.lua file.foobar`.

local grammar = vim.env.FOOBAR_GRAMMAR or vim.fn.expand("~/git_repos/lsp/tree-sitter-foobar")

vim.filetype.add({ extension = { foobar = "foobar" } })

-- Highlighting: the compiled parser plus the grammar's own highlights.scm.
vim.treesitter.language.add("foobar", { path = grammar .. "/parser.so" })
vim.treesitter.query.set(
  "foobar",
  "highlights",
  table.concat(vim.fn.readfile(grammar .. "/queries/highlights.scm"), "\n")
)

vim.api.nvim_create_autocmd("FileType", {
  pattern = "foobar",
  callback = function(args)
    vim.treesitter.start(args.buf, "foobar")
    vim.bo[args.buf].commentstring = "# %s"
  end,
})

-- Language server: `foobar` must be on PATH (`cargo install --path .` in foobar-language-server).
vim.lsp.config("foobar", {
  cmd = { "foobar", "lsp" },
  filetypes = { "foobar" },
  root_markers = { ".git" },
})
vim.lsp.enable("foobar")
