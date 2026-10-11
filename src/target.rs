//! What the node under the cursor is, in the language's own terms.

use ropey::Rope;
use tree_sitter::Node;
use tree_sitter_foobar::kind;

use crate::symbol::template_param;
use crate::text::RopeExt;

pub enum Target<'t> {
    /// `defines`, `process`, `template`, `use` or `include`.
    Keyword(String),
    /// `x` in `%(x)` naming a define.
    DefineUse(String),
    /// `x` in `x: "..."` inside `defines`.
    DefineKey(String),
    /// `k` in `k: "..."` inside a process or template.
    EntryKey(String),
    /// A template parameter: `p` in `template t(p)`, or `%(p)` in that template's body.
    Param {
        name: String,
        decl: Node<'t>,
        template: Node<'t>,
    },
    /// `t` in `template t(...)`.
    TemplateName(String),
    /// `t` in `use t(...)`.
    TemplateCall(String),
    /// `x` in `process x`.
    ProcessName(String),
    /// `p` in `include p`.
    IncludePath(String),
}

/// What `node` (from `Document::node_at`) stands for, if anything.
pub fn target<'t>(node: Node<'t>, rope: &Rope) -> Option<Target<'t>> {
    let parent = node.parent()?;
    let text = || rope.node_text(node);
    if !node.is_named() {
        // The only anonymous children of these nodes are their keywords.
        let keyword = matches!(
            parent.kind(),
            kind::SECTION_DEFINES
                | kind::SECTION_PROCESS
                | kind::SECTION_TEMPLATE
                | kind::USE
                | kind::INCLUDE
        );
        return keyword.then(|| Target::Keyword(text()));
    }
    Some(match (node.kind(), parent.kind()) {
        (kind::IDENTIFIER, kind::VARIABLE) => match template_param(node, rope) {
            Some(decl) => Target::Param {
                name: text(),
                template: decl.parent()?.parent()?,
                decl,
            },
            None => Target::DefineUse(text()),
        },
        (kind::IDENTIFIER, kind::PARAMETERS) => Target::Param {
            name: text(),
            decl: node,
            template: parent.parent()?,
        },
        (kind::IDENTIFIER, kind::ENTRY) if parent.parent()?.kind() == kind::SECTION_DEFINES => {
            Target::DefineKey(text())
        }
        (kind::IDENTIFIER, kind::ENTRY) => Target::EntryKey(text()),
        (kind::IDENTIFIER, kind::SECTION_TEMPLATE) => Target::TemplateName(text()),
        (kind::IDENTIFIER, kind::USE) => Target::TemplateCall(text()),
        (kind::NAME, kind::SECTION_PROCESS) => Target::ProcessName(text().trim().to_string()),
        (kind::PATH, kind::INCLUDE) => Target::IncludePath(text().trim().to_string()),
        _ => return None,
    })
}
