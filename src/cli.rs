//! Command-line tools: `foobar lint` and `foobar fmt`.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, ensure};
use ropey::Rope;
use tower_lsp_server::ls_types::{Diagnostic, DiagnosticSeverity, NumberOrString};

use crate::diagnostics;
use crate::format;
use crate::text::RopeExt;
use crate::workspace::{Workspace, file_key, walk_foobar_files};

/// Lint `paths` (default: all of `root`). Exit code 1 on any error.
///
/// `root` is always scanned: lookups and reference counts need the whole workspace.
pub fn lint(root: &Path, paths: &[PathBuf]) -> Result<ExitCode> {
    ensure!(root.is_dir(), "not a directory: {}", root.display());

    let ws = Workspace::default();
    ws.set_roots(vec![root.to_path_buf()]);
    ws.scan_workspace();

    let targets = lint_targets(root, paths)?;
    let (mut errors, mut warnings) = (0, 0);
    for path in &targets {
        // Scanned already if under `root`, else read now.
        let file = ws
            .load(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let mut diags = diagnostics::compute(&ws, &file);
        if diags.is_empty() {
            continue;
        }
        diags.sort_by_key(|d| (d.range.start.line, d.range.start.character));
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let rope = Rope::from_str(&text);
        for d in &diags {
            if d.severity == Some(DiagnosticSeverity::ERROR) {
                errors += 1;
            } else {
                warnings += 1;
            }
            println!("{}", report_line(&shown(path), &rope, d));
        }
    }
    eprintln!(
        "{errors} errors, {warnings} warnings in {} files",
        targets.len()
    );
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

/// Format `paths` in place, skipping files with syntax errors.
///
/// `check` writes nothing and lists the files that would change (exit code 1).
pub fn fmt(paths: &[PathBuf], check: bool) -> Result<ExitCode> {
    let (dirs, files): (Vec<_>, Vec<_>) = paths.iter().cloned().partition(|p| p.is_dir());

    let mut failed = false;
    for path in files.into_iter().chain(walk_foobar_files(&dirs)) {
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let Some(formatted) = format::format(&text) else {
            eprintln!("{}: syntax errors, not formatted", shown(&path).display());
            failed = true;
            continue;
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

/// `path:line:col: severity: message [code]`, 1-based, columns in characters (not UTF-16).
fn report_line(path: &Path, rope: &Rope, d: &Diagnostic) -> String {
    let pos = rope.lsp_position_to_text_pos(d.range.start);
    let col = pos.char - rope.line_to_char(pos.point.row) + 1;
    let severity = match d.severity {
        Some(DiagnosticSeverity::ERROR) => "error",
        Some(DiagnosticSeverity::WARNING) => "warning",
        Some(DiagnosticSeverity::INFORMATION) => "info",
        _ => "hint",
    };
    let code = match &d.code {
        Some(NumberOrString::String(c)) => format!(" [{c}]"),
        Some(NumberOrString::Number(n)) => format!(" [{n}]"),
        None => String::new(),
    };
    format!(
        "{}:{}:{col}: {severity}: {}{code}",
        path.display(),
        pos.point.row + 1,
        d.message
    )
}

/// `path` relative to the current directory, if below it.
fn shown(path: &Path) -> PathBuf {
    let cwd = std::env::current_dir().and_then(|d| d.canonicalize());
    match cwd {
        Ok(cwd) => path.strip_prefix(&cwd).unwrap_or(path).to_path_buf(),
        Err(_) => path.to_path_buf(),
    }
}
