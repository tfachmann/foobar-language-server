; keywords
"process" @keyword
"defines" @keyword
"include" @keyword.import
"template" @keyword.function
"use" @keyword

; punctuation
":" @punctuation.delimiter
"," @punctuation.delimiter
(variable ["%(" ")"] @punctuation.special)
(parameters ["(" ")"] @punctuation.bracket)
(arguments ["(" ")"] @punctuation.bracket)

; identifiers
(entry (identifier) @property)
(variable (identifier) @variable)

; process header name
(section_process (name) @function)

; templates: definition, call site, parameters
(section_template (identifier) @function)
(use (identifier) @function.call)
(parameters (identifier) @variable.parameter)
(argument (identifier) @variable.parameter)

; include path
(include (path) @string.special.path)

; literals
(string) @string
(raw_string) @string

; comments
(comment) @comment
