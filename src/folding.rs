use tower_lsp_server::ls_types::{FoldingRange, FoldingRangeKind};

use crate::document::Document;
use crate::text::RopeExt;

/// One fold per top-level node that spans more than one line (sections).
pub fn ranges(doc: &Document) -> Vec<FoldingRange> {
    let root = doc.tree.root_node();
    let mut cursor = root.walk();
    root.named_children(&mut cursor)
        .map(|node| doc.rope.node_range(node))
        .filter(|range| range.end.line > range.start.line)
        .map(|range| FoldingRange {
            start_line: range.start.line,
            end_line: range.end.line,
            kind: Some(FoldingRangeKind::Region),
            ..FoldingRange::default()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_fold_per_multi_line_section() {
        let text = "defines\n  a: \"1\"\n  b: \"2\"\n\ninclude ./x.foobar\n\nprocess p\n  command: \"%(a)\"\n";
        let doc = Document::from_text("file:///t.foobar".parse().unwrap(), text);
        let folds: Vec<_> = ranges(&doc)
            .iter()
            .map(|f| (f.start_line, f.end_line))
            .collect();
        assert_eq!(folds, vec![(0, 2), (6, 7)]);
    }
}
