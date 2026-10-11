# foobar language reference

Hover text for keywords, process keys and built-in variables comes from this file: every `## name`
section under one of the `#` headings below. Code blocks are examples; the server shows them as
written.

# Keywords

## defines

`defines`

Opens a section of `key: value` entries. Any value can use an entry as `%(key)`, in this file and
in every file that includes it.

```foobar
defines
  # shared by every process below
  name: "eurorust"
  slides: "%(name).typ"
```

## process

`process <name>`

Starts a named process. Its entries say what runs and where (see the process keys). The name runs
to the end of the line and may contain spaces.

```foobar
process compile presentation
  command: "typst compile %(slides)"
  cwd: 'talk'
```

## template

`template <name>(<param>, ...)`

Defines reusable process entries. Inside the template, `%(param)` means the parameter; a `use`
call fills it in.

```foobar
template compile(src, dst)
  command: "typst compile %(src) %(dst)"
```

## use

`use <template>(<param>: <value>, ...)`

Inserts a template's entries into a process. Every parameter must be passed, by name.

```foobar
process slides
  use compile(src: "%(name).typ", dst: "%(name).pdf")
```

## include

`include <path>`

Makes the defines and templates of another file available. The path is relative to the directory
of the including file.

```foobar
include common.foobar
```

# Process keys

Entries of a `process`. They are also valid inside a `template`.

## command

The shell command line that starts the process.

```foobar
process slides
  command: "typst compile %(name).typ"
```

## node

The machine the process runs on. Without it, the process runs where the configuration is started.

```foobar
process render
  node: 'render-01'
  command: "blender --background scene.blend"
```

## note

A short description, shown next to the process in a process list.

```foobar
process slides
  note: 'recompiles the slides on every change'
  command: "typst watch slides.typ"
```

## cwd

The working directory, relative to the directory of this file. Defaults to that directory.

```foobar
process slides
  cwd: 'talk'
  command: "typst compile slides.typ"
```

## env

Extra environment variables, as space-separated `NAME=value` pairs.

```foobar
process server
  env: "RUST_LOG=debug PORT=8080"
  command: "cargo run --release"
```

## log_level

How much the process manager reports about this process: `error`, `warn`, `info` or `debug`.

```foobar
process tests
  log_level: 'debug'
  command: "cargo test"
```

## depends_on

The name of another process that must be running before this one starts.

```foobar
process viewer
  depends_on: 'compile presentation'
  command: "zathura slides.pdf"
```

# Built-in variables

Available in every value as `%(name)`, without a `defines` entry. A define with the same name hides
the built-in.

## curdir

The absolute path of the directory that contains this file.

```foobar
process list
  command: "ls %(curdir)"
```

## hostname

The name of the machine the process runs on, resolved when the process starts.

```foobar
process hello
  note: "runs on %(hostname)"
  command: "echo hello from %(hostname)"
```
