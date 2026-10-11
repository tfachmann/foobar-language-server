//! The language reference: keywords, process keys and built-in variables.
//!
//! The text lives in `docs/language.md` (compiled in).

use std::sync::LazyLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Group {
    Keyword,
    /// Closed set: a process or template entry with any other key is an error.
    ProcessKey,
    Builtin,
}

impl Group {
    const ALL: [Group; 3] = [Group::Keyword, Group::ProcessKey, Group::Builtin];

    /// The `# heading` of the group in `docs/language.md`.
    fn heading(self) -> &'static str {
        match self {
            Group::Keyword => "Keywords",
            Group::ProcessKey => "Process keys",
            Group::Builtin => "Built-in variables",
        }
    }

    /// `keyword`, `process key`, `built-in variable`.
    pub fn label(self) -> &'static str {
        match self {
            Group::Keyword => "keyword",
            Group::ProcessKey => "process key",
            Group::Builtin => "built-in variable",
        }
    }
}

/// One `## name` section of `docs/language.md`.
#[derive(Debug)]
pub struct Doc {
    pub group: Group,
    pub name: String,
    /// A first paragraph that is only a code span, e.g. `process <name>`.
    pub usage: Option<String>,
    pub description: String,
    /// The first code block.
    pub example: Option<String>,
}

impl Doc {
    pub fn hover(&self) -> String {
        format!(
            "**{}**\n---\n{}\n---\n{}",
            self.name,
            self.body(),
            self.group.label()
        )
    }

    pub fn documentation(&self) -> String {
        self.with_example("\n")
    }

    /// Hover hides fence rows entirely, so it needs the blank line.
    fn body(&self) -> String {
        match &self.usage {
            Some(usage) => format!("`{usage}`\n\n{}", self.with_example("\n\n")),
            None => self.with_example("\n\n"),
        }
    }

    fn with_example(&self, gap: &str) -> String {
        match &self.example {
            Some(example) => format!("{}{gap}```foobar\n{example}\n```", self.description),
            None => self.description.clone(),
        }
    }
}

static DOCS: LazyLock<Vec<Doc>> = LazyLock::new(|| parse(include_str!("../docs/language.md")));

pub fn get(group: Group, name: &str) -> Option<&'static Doc> {
    DOCS.iter().find(|d| d.group == group && d.name == name)
}

/// The docs of `group`, sorted by name.
pub fn all(group: Group) -> Vec<&'static Doc> {
    let mut docs: Vec<&Doc> = DOCS.iter().filter(|d| d.group == group).collect();
    docs.sort_by(|a, b| a.name.cmp(&b.name));
    docs
}

pub fn hover(group: Group, name: &str) -> Option<String> {
    get(group, name).map(Doc::hover)
}

pub fn is_builtin(name: &str) -> bool {
    get(Group::Builtin, name).is_some()
}

pub fn is_process_key(name: &str) -> bool {
    get(Group::ProcessKey, name).is_some()
}

fn parse(text: &str) -> Vec<Doc> {
    let mut docs = Vec::new();
    let mut group = None;
    let mut name: Option<String> = None;
    let mut body = String::new();
    let mut in_code = false;
    for line in text.lines().chain(["# end"]) {
        if line.starts_with("```") {
            in_code = !in_code;
        }
        let heading = match (in_code, line.strip_prefix("# "), line.strip_prefix("## ")) {
            (false, Some(title), _) => Some((1, title)),
            (false, _, Some(title)) => Some((2, title)),
            _ => None,
        };
        let Some((level, title)) = heading else {
            body.push_str(line);
            body.push('\n');
            continue;
        };
        if let (Some(group), Some(name)) = (group, name.take()) {
            docs.push(section(group, name, &body));
        }
        body.clear();
        if level == 1 {
            group = Group::ALL.into_iter().find(|g| g.heading() == title.trim());
        } else {
            name = Some(title.trim().to_string());
        }
    }
    docs
}

/// Splits a section body into usage line, prose and the first code block.
fn section(group: Group, name: String, body: &str) -> Doc {
    let mut prose = String::new();
    let mut example = None;
    let mut code: Option<Vec<&str>> = None;
    for line in body.lines() {
        match (code.as_mut(), line.starts_with("```")) {
            (None, true) => code = Some(Vec::new()),
            (Some(lines), true) => {
                example.get_or_insert_with(|| lines.join("\n"));
                code = None;
            }
            (Some(lines), false) => lines.push(line),
            (None, false) => {
                prose.push_str(line);
                prose.push('\n');
            }
        }
    }
    let prose = prose.trim();
    let is_usage =
        |p: &str| p.len() > 2 && p.starts_with('`') && p.ends_with('`') && !p.contains('\n');
    let (usage, description) = match prose.split_once("\n\n") {
        Some((first, rest)) if is_usage(first) => (Some(first), rest),
        None if is_usage(prose) => (Some(prose), ""),
        _ => (None, prose),
    };
    Doc {
        group,
        name,
        usage: usage.map(|u| u.trim_matches('`').to_string()),
        description: description.trim().to_string(),
        example,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter_foobar::keyword;

    /// Every keyword of the grammar is documented, and sections split into their parts.
    #[test]
    fn reference_covers_the_grammar() {
        for word in [
            keyword::DEFINES,
            keyword::PROCESS,
            keyword::TEMPLATE,
            keyword::USE,
            keyword::INCLUDE,
        ] {
            assert!(get(Group::Keyword, word).is_some(), "{word} undocumented");
        }
        let defines = get(Group::Keyword, keyword::DEFINES).unwrap();
        assert_eq!(defines.usage.as_deref(), Some("defines"));
        assert!(
            defines
                .description
                .starts_with("Opens a section of `key: value` entries.")
        );
        // Comments in examples are not headings.
        let example = defines.example.as_deref().unwrap();
        assert!(example.contains("# shared by every process below"));
        let builtins: Vec<&str> = all(Group::Builtin)
            .iter()
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(builtins, ["curdir", "hostname"]);
        assert!(is_process_key("command"));
    }
}
