//! The language reference: keywords, process keys and built-in variables.
//!
//! The text lives in `docs/language.md` (compiled in).

use std::collections::HashMap;
use std::sync::LazyLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Group {
    Keyword,
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

    fn label(self) -> &'static str {
        match self {
            Group::Keyword => "keyword",
            Group::ProcessKey => "process key",
            Group::Builtin => "built-in variable",
        }
    }
}

static DOCS: LazyLock<HashMap<(Group, String), String>> =
    LazyLock::new(|| parse(include_str!("../docs/language.md")));

pub fn hover(group: Group, name: &str) -> Option<String> {
    let body = DOCS.get(&(group, name.to_string()))?;
    Some(format!("**{name}**\n---\n{body}\n---\n{}", group.label()))
}

pub fn names(group: Group) -> Vec<&'static str> {
    let mut names: Vec<&str> = DOCS
        .keys()
        .filter(|(g, _)| *g == group)
        .map(|(_, name)| name.as_str())
        .collect();
    names.sort();
    names
}

pub fn is_builtin(name: &str) -> bool {
    DOCS.contains_key(&(Group::Builtin, name.to_string()))
}

fn parse(text: &str) -> HashMap<(Group, String), String> {
    let mut docs = HashMap::new();
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
            docs.insert((group, name), body.trim().to_string());
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

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter_foobar::keyword;

    /// Every keyword of the grammar is documented, and example comments are not headings.
    #[test]
    fn reference_covers_the_grammar() {
        for word in [
            keyword::DEFINES,
            keyword::PROCESS,
            keyword::TEMPLATE,
            keyword::USE,
            keyword::INCLUDE,
        ] {
            assert!(hover(Group::Keyword, word).is_some(), "{word} undocumented");
        }
        assert!(
            hover(Group::Keyword, keyword::DEFINES)
                .unwrap()
                .contains("# shared by every process below")
        );
        assert_eq!(names(Group::Builtin), ["curdir", "hostname"]);
        assert!(names(Group::ProcessKey).contains(&"command"));
    }
}
