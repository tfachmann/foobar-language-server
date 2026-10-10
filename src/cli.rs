//! Command-line tools: `foobar lint` and `foobar fmt`.

use std::collections::BTreeSet;
use std::fs;
use std::io::IsTerminal;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, ensure};
use ariadne::{Color, Config, Label, Report, ReportKind, Source};
use clap::ValueEnum;
use ropey::Rope;
use serde_json::{Value, json};
use tower_lsp_server::ls_types::{Diagnostic, DiagnosticSeverity, NumberOrString};

use crate::diagnostics;
use crate::format::Formatter;
use crate::text::{RopeExt, TextPos};
use crate::workspace::{Workspace, file_key, walk_foobar_files};

/// How `lint` prints findings. Names follow cargo's `--message-format`.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum MessageFormat {
    Human,
    Short,
    Json,
}

/// A linted file with its findings.
struct FileReport {
    /// Relative to the current directory when below it.
    path: PathBuf,
    text: String,
    findings: Vec<Finding>,
}

/// One diagnostic, converted once from LSP positions for every output format.
struct Finding {
    severity: &'static str,
    code: Option<String>,
    message: String,
    /// 1-based `(line, column)`, columns in characters (not UTF-16).
    start: (usize, usize),
    end: (usize, usize),
    /// Character offsets into the file.
    span: Range<usize>,
}

impl Finding {
    fn new(rope: &Rope, d: &Diagnostic) -> Self {
        let start = rope.lsp_position_to_text_pos(d.range.start);
        let end = rope.lsp_position_to_text_pos(d.range.end);
        let line_col = |p: TextPos| (p.point.row + 1, p.char - rope.line_to_char(p.point.row) + 1);
        Self {
            severity: match d.severity {
                Some(DiagnosticSeverity::ERROR) => "error",
                Some(DiagnosticSeverity::WARNING) => "warning",
                Some(DiagnosticSeverity::INFORMATION) => "info",
                _ => "hint",
            },
            code: match &d.code {
                Some(NumberOrString::String(c)) => Some(c.clone()),
                Some(NumberOrString::Number(n)) => Some(n.to_string()),
                None => None,
            },
            message: d.message.clone(),
            start: line_col(start),
            end: line_col(end),
            span: start.char..end.char,
        }
    }
}

/// Lint `paths` (default: all of `root`). Exit code 1 on any error.
///
/// `root` is always scanned: lookups and reference counts need the whole workspace.
pub fn lint(root: &Path, paths: &[PathBuf], format: MessageFormat) -> Result<ExitCode> {
    ensure!(root.is_dir(), "not a directory: {}", root.display());

    let ws = Workspace::default();
    ws.set_roots(vec![root.to_path_buf()]);
    ws.scan_workspace();

    let targets = lint_targets(root, paths)?;
    let mut reports = Vec::new();
    for path in &targets {
        // Scanned already if under `root`, else read now.
        let file = ws
            .load(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let diags = diagnostics::compute(&ws, &file);
        if diags.is_empty() {
            continue;
        }
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let rope = Rope::from_str(&text);
        let mut findings: Vec<Finding> = diags.iter().map(|d| Finding::new(&rope, d)).collect();
        findings.sort_by_key(|f| f.start);
        reports.push(FileReport {
            path: shown(path),
            text,
            findings,
        });
    }

    match format {
        MessageFormat::Human => print_human(&reports)?,
        MessageFormat::Short => print_short(&reports),
        MessageFormat::Json => println!("{}", serde_json::to_string_pretty(&to_json(&reports))?),
    }

    let findings = || reports.iter().flat_map(|r| &r.findings);
    let errors = findings().filter(|f| f.severity == "error").count();
    let warnings = findings().count() - errors;
    // JSON consumers often capture stderr too; the array already holds the counts.
    if !matches!(format, MessageFormat::Json) {
        eprintln!(
            "{}, {} in {}",
            count(errors, "error"),
            count(warnings, "warning"),
            count(targets.len(), "file")
        );
    }
    Ok(if errors > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// Sorted workspace keys of `paths` (default: `root`), directories expanded to `*.foobar` files.
fn lint_targets(root: &Path, paths: &[PathBuf]) -> Result<BTreeSet<PathBuf>> {
    let paths = if paths.is_empty() {
        vec![root.to_path_buf()]
    } else {
        paths.to_vec()
    };
    let mut targets = BTreeSet::new();
    for path in paths {
        ensure!(
            path.exists(),
            "no such file or directory: {}",
            path.display()
        );
        if path.is_dir() {
            targets.extend(walk_foobar_files(&[path]).iter().map(|p| file_key(p)));
        } else {
            targets.insert(file_key(&path));
        }
    }
    Ok(targets)
}

/// ariadne reports on stdout; colored only on a terminal without `NO_COLOR`.
fn print_human(reports: &[FileReport]) -> Result<()> {
    let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    for report in reports {
        let id = report.path.display().to_string();
        let source = Source::from(report.text.as_str());
        for f in &report.findings {
            let (kind, label_color) = match f.severity {
                "error" => (ReportKind::Error, Color::Red),
                "warning" => (ReportKind::Warning, Color::Yellow),
                _ => (ReportKind::Advice, Color::Blue),
            };
            // ariadne only draws a label's underline and arrow when it has a message.
            let label = Label::new((id.clone(), f.span.clone()))
                .with_message(&f.message)
                .with_color(label_color);
            let mut builder = Report::build(kind, (id.clone(), f.span.clone()))
                .with_config(Config::default().with_color(color))
                .with_message(&f.message)
                .with_label(label);
            if let Some(code) = &f.code {
                builder = builder.with_code(code);
            }
            builder.finish().print((id.clone(), &source))?;
        }
    }
    Ok(())
}

/// `path:line:col: severity: message [code]`, as in cargo's short format.
fn print_short(reports: &[FileReport]) {
    for report in reports {
        for f in &report.findings {
            let code = f
                .code
                .as_ref()
                .map(|c| format!(" [{c}]"))
                .unwrap_or_default();
            println!(
                "{}:{}:{}: {}: {}{code}",
                report.path.display(),
                f.start.0,
                f.start.1,
                f.severity,
                f.message
            );
        }
    }
}

/// One flat array of findings; positions as in the short format.
fn to_json(reports: &[FileReport]) -> Value {
    reports
        .iter()
        .flat_map(|report| {
            report.findings.iter().map(move |f| {
                json!({
                    "path": report.path.display().to_string(),
                    "severity": f.severity,
                    "code": f.code,
                    "message": f.message,
                    "start": { "line": f.start.0, "column": f.start.1 },
                    "end": { "line": f.end.0, "column": f.end.1 },
                })
            })
        })
        .collect()
}

/// Format `paths` in place, skipping files with syntax errors.
///
/// `check` writes nothing and lists the files that would change (exit code 1).
pub fn fmt(paths: &[PathBuf], check: bool, formatter: &Formatter) -> Result<ExitCode> {
    let (dirs, files): (Vec<_>, Vec<_>) = paths.iter().cloned().partition(|p| p.is_dir());

    let mut files: Vec<PathBuf> = files.into_iter().chain(walk_foobar_files(&dirs)).collect();
    files.sort();

    let mut failed = false;
    for path in files {
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let formatted = match formatter.format(&text) {
            Ok(Some(formatted)) => formatted,
            Ok(None) => {
                eprintln!("{}: syntax errors, not formatted", shown(&path).display());
                failed = true;
                continue;
            }
            Err(e) => {
                eprintln!("{}: {e:#}, not formatted", shown(&path).display());
                failed = true;
                continue;
            }
        };
        if formatted == text {
            continue;
        }
        if check {
            println!("{}", shown(&path).display());
            failed = true;
        } else {
            fs::write(&path, formatted).with_context(|| format!("writing {}", path.display()))?;
        }
    }
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// `1 error`, `2 errors`.
fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// `path` relative to the current directory, if below it.
fn shown(path: &Path) -> PathBuf {
    let cwd = std::env::current_dir().and_then(|d| d.canonicalize());
    match cwd {
        Ok(cwd) => path.strip_prefix(&cwd).unwrap_or(path).to_path_buf(),
        Err(_) => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::ls_types::{Position, Range as LspRange};

    #[test]
    fn finding_columns_count_characters() {
        // `😀` is two UTF-16 code units but one character.
        let rope = Rope::from_str("a\n😀 %(x)\n");
        let d = Diagnostic {
            range: LspRange::new(Position::new(1, 5), Position::new(1, 6)),
            severity: Some(DiagnosticSeverity::ERROR),
            message: "undefined variable: x".to_string(),
            ..Diagnostic::default()
        };
        let f = Finding::new(&rope, &d);
        assert_eq!((f.start, f.end), ((2, 5), (2, 6)));
        assert_eq!(f.span, 6..7);
    }
}
