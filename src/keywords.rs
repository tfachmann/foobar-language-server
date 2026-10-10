//! Static documentation for keywords and well-known entry keys.
//!
//! NOTE: Should be loaded from a file, and more extensive (e.g. including examples)

pub fn keyword_doc(name: &str) -> Option<&'static str> {
    match name {
        "defines" => Some(
            "**defines** - opens a section of `key: value` entries.\n\n\
             Entries declared here can be referenced from any value using `%(key)`.",
        ),
        "process" => Some(
            "**process** `<name>` - opens a named process section.\n\n\
             Followed by entries that configure the process (e.g. `command:`).",
        ),
        "template" => Some(
            "**template** `<name>(<params>)` - defines reusable process entries.\n\n\
             Parameters are referenced as `%(param)` inside the template body.",
        ),
        "use" => Some(
            "**use** `<template>(<param>: <value>, ...)` - inserts a template's entries into a process.\n\n\
             Every template parameter must be passed.",
        ),
        "include" => Some(
            "**include** `<path>` - pulls another foobar file into the symbol scope.\n\n\
             Path is relative to the including file's directory.",
        ),
        _ => None,
    }
}

pub fn process_entry_key_doc(name: &str) -> Option<&'static str> {
    match name {
        "command" => Some(
            "**command** - the shell command line to run for this process.\n\n\
             Supports `%(name)` variable interpolation.",
        ),
        _ => None,
    }
}
