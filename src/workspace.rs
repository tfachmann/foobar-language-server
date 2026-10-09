//! Shared workspace state
//! - open documents
//! - disk cache of included files
//!
//! Handles transitive symbol lookup with cycle detection.

use std::collections::{HashSet, VecDeque};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use dashmap::DashMap;
use ropey::Rope;
use tokio::sync::RwLock;
use tower_lsp_server::ls_types::{Diagnostic, Uri};
use tree_sitter::Parser;

use crate::diagnostics::collect_parse_errors;
use crate::document::Document;
use crate::symbol::{DefineEntry, IncludeRef, SymbolIndex};
use crate::text::{path_to_uri, uri_to_path};

/// One file that the LSP knows about via the filesystem (not opened in the editor).
pub struct DiskFile {
    pub mtime: Option<SystemTime>,
    pub symbols: SymbolIndex,
    /// Parse errors precomputed at load time.
    pub parse_diags: Vec<Diagnostic>,
}

/// Where a definition was found.
#[derive(Clone, Debug)]
pub enum DefineSource {
    OpenDoc(Uri),
    Disk(PathBuf),
}

/// A successful symbol resolution.
#[derive(Clone, Debug)]
pub struct DefineHit {
    pub entry: DefineEntry,
    pub source: DefineSource,
}

/// Outcome of resolving a raw include path.
#[derive(Clone, Debug)]
pub struct ResolvedInclude {
    /// Path joined relative to the including file's directory; may or may not exist.
    pub path: PathBuf,
    /// True if `path.exists()` at resolution time.
    pub exists: bool,
}

/// A file reached by following `include`s.
pub struct Reached {
    pub path: PathBuf,
    /// Snapshot of the file's symbols. `None` if the file is missing or unreadable.
    pub file: Option<(DefineSource, SymbolIndex)>,
}

/// Open documents, disk-cached included files, and the include graph between them.
#[derive(Default)]
pub struct Workspace {
    /// Files open in the editor (via LSP).
    pub docs: DashMap<Uri, Arc<RwLock<Document>>>,
    /// Cache of files read from disk (not currently open in the editor).
    pub disk: DashMap<PathBuf, Arc<DiskFile>>,
    /// Reverse include graph: file path -> open docs that (transitively) include it.
    pub dependents: DashMap<PathBuf, HashSet<Uri>>,
    /// Workspace root directories captured at LSP `initialize`.
    pub roots: OnceLock<Vec<PathBuf>>,
}

impl Workspace {
    pub fn set_roots(&self, roots: Vec<PathBuf>) {
        let _ = self.roots.set(roots);
    }

    /// The open document for `uri`.
    ///
    /// Clones the `Arc` out of the map so no DashMap shard guard is held across a later `.await`.
    pub fn doc(&self, uri: &Uri) -> Option<Arc<RwLock<Document>>> {
        self.docs.get(uri).map(|e| e.value().clone())
    }

    /// The open document whose canonical path equals `path`, if any.
    pub fn doc_at(&self, path: &Path) -> Option<Arc<RwLock<Document>>> {
        self.docs
            .iter()
            .find(|e| {
                uri_to_path(e.key())
                    .and_then(|p| p.canonicalize().ok())
                    .as_deref()
                    == Some(path)
            })
            .map(|e| e.value().clone())
    }

    /// Recursively walk every stored root, parsing each `*.foobar` file into `self.disk`.
    pub fn scan_workspace(&self) {
        let Some(roots) = self.roots.get() else {
            return;
        };
        for path in walk_foobar_files(roots) {
            if self.doc_at(&path).is_none() {
                self.load_if_stale(&path);
            }
        }
    }

    /// Resolve a raw include path string against a base `file://` URI.
    ///
    /// Returns `None` for non-file base URIs.
    pub fn resolve(&self, base: &Uri, raw: &str) -> Option<ResolvedInclude> {
        let base_path = uri_to_path(base)?;
        let joined = base_path.parent()?.join(raw.trim());
        let path = joined.canonicalize().unwrap_or(joined);
        let exists = path.exists();
        Some(ResolvedInclude { path, exists })
    }

    /// Breadth-first walk over everything transitively included from `base`.
    ///
    /// Open documents win over the disk cache; stale or missing cache entries are (re)loaded.
    /// Each path is visited once, so include cycles terminate.
    pub async fn reachable(&self, base: &Uri, includes: &[IncludeRef]) -> Vec<Reached> {
        let mut out: Vec<Reached> = Vec::new();
        let mut visited: HashSet<PathBuf> = HashSet::new();
        let mut queue: VecDeque<(Uri, Vec<IncludeRef>)> = VecDeque::new();
        queue.push_back((base.clone(), includes.to_vec()));

        while let Some((base, includes)) = queue.pop_front() {
            for inc in includes {
                let Some(resolved) = self.resolve(&base, &inc.raw_path) else {
                    continue;
                };
                if !visited.insert(resolved.path.clone()) {
                    continue;
                }
                let file = if !resolved.exists {
                    None
                } else if let Some(doc) = self.doc_at(&resolved.path) {
                    let doc = doc.read().await;
                    Some((DefineSource::OpenDoc(doc.uri.clone()), doc.symbols.clone()))
                } else {
                    self.load_if_stale(&resolved.path)
                        .map(|f| (DefineSource::Disk(resolved.path.clone()), f.symbols.clone()))
                };
                if let Some((source, symbols)) = &file {
                    let next_base = match source {
                        DefineSource::OpenDoc(uri) => Some(uri.clone()),
                        DefineSource::Disk(path) => path_to_uri(path),
                    };
                    queue.push_back((next_base.unwrap_or(base.clone()), symbols.includes.clone()));
                }
                out.push(Reached {
                    path: resolved.path,
                    file,
                });
            }
        }
        out
    }

    /// Load all transitive includes of `doc` into the disk cache and record `doc` as their dependent.
    pub async fn refresh_includes_for(&self, doc: &Document) {
        let reached = self.reachable(&doc.uri, &doc.symbols.includes).await;
        self.forget_doc(&doc.uri);
        for r in reached {
            self.dependents
                .entry(r.path)
                .or_default()
                .insert(doc.uri.clone());
        }
    }

    /// Drop `uri` from dependents entirely (e.g. on `did_close`).
    pub fn forget_doc(&self, uri: &Uri) {
        for mut entry in self.dependents.iter_mut() {
            entry.value_mut().remove(uri);
        }
        self.dependents.retain(|_, set| !set.is_empty());
    }

    /// Return the set of open-doc URIs that transitively include `path`.
    pub fn dependents_of(&self, path: &Path) -> Vec<Uri> {
        self.dependents
            .get(path)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Return the cached `DiskFile` for `path`, re-reading it if missing or modified on disk.
    fn load_if_stale(&self, path: &Path) -> Option<Arc<DiskFile>> {
        let mtime = fs::metadata(path).and_then(|m| m.modified()).ok();
        if let Some(cached) = self.disk.get(path)
            && cached.mtime == mtime
        {
            return Some(cached.clone());
        }
        let loaded = Arc::new(load_disk_file(path, mtime)?);
        self.disk.insert(path.to_path_buf(), loaded.clone());
        Some(loaded)
    }

    /// Find the define for `name`: the doc itself first, then its transitive includes.
    pub async fn lookup_define(&self, doc: &Document, name: &str) -> Option<DefineHit> {
        if let Some(entry) = doc.symbols.defines.get(name) {
            return Some(DefineHit {
                entry: entry.clone(),
                source: DefineSource::OpenDoc(doc.uri.clone()),
            });
        }
        self.reachable(&doc.uri, &doc.symbols.includes)
            .await
            .into_iter()
            .find_map(|r| {
                let (source, symbols) = r.file?;
                let entry = symbols.defines.get(name)?.clone();
                Some(DefineHit { entry, source })
            })
    }

    /// Does `name` resolve from the file at `base_uri` with symbols `local`?
    ///
    /// Checks the file itself, its transitive includes, and finally every file in the workspace.
    pub async fn has_define(&self, base_uri: &Uri, local: &SymbolIndex, name: &str) -> bool {
        if local.defines.contains_key(name) {
            return true;
        }
        let via_include = self
            .reachable(base_uri, &local.includes)
            .await
            .iter()
            .filter_map(|r| r.file.as_ref())
            .any(|(_, symbols)| symbols.defines.contains_key(name));
        if via_include {
            return true;
        }

        // Workspace-wide pool fallback.
        let doc_arcs: Vec<Arc<RwLock<Document>>> =
            self.docs.iter().map(|e| e.value().clone()).collect();
        for doc_arc in doc_arcs {
            if doc_arc.read().await.symbols.defines.contains_key(name) {
                return true;
            }
        }
        self.disk
            .iter()
            .any(|e| e.value().symbols.defines.contains_key(name))
    }

    /// Workspace-wide count of `%(name)` references.
    pub async fn count_refs(&self, name: &str) -> usize {
        let count =
            |symbols: &SymbolIndex| symbols.var_refs.iter().filter(|(k, _)| k == name).count();

        let mut n = 0;
        let mut doc_paths: HashSet<PathBuf> = HashSet::new();
        let doc_arcs: Vec<Arc<RwLock<Document>>> =
            self.docs.iter().map(|e| e.value().clone()).collect();
        for doc_arc in doc_arcs {
            let doc = doc_arc.read().await;
            n += count(&doc.symbols);
            if let Some(p) = uri_to_path(&doc.uri).and_then(|p| p.canonicalize().ok()) {
                doc_paths.insert(p);
            }
        }
        // Open buffers shadow their on-disk copy.
        n + self
            .disk
            .iter()
            .filter(|e| !doc_paths.contains(e.key()))
            .map(|e| count(&e.value().symbols))
            .sum::<usize>()
    }

    /// Number of defines in a resolved include target (open doc first, then disk).
    ///
    /// Returns `None` if the file isn't loaded (neither open nor cached).
    pub async fn defines_count_at(&self, path: &Path) -> Option<usize> {
        if let Some(doc) = self.doc_at(path) {
            return Some(doc.read().await.symbols.defines.len());
        }
        self.disk.get(path).map(|f| f.symbols.defines.len())
    }
}

/// Read + parse + index a file from disk. `None` on any IO/parse failure.
fn load_disk_file(path: &Path, mtime: Option<SystemTime>) -> Option<DiskFile> {
    let text = fs::read_to_string(path).ok()?;
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_foobar::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(&text, None)?;
    let rope = Rope::from_str(&text);
    let symbols = SymbolIndex::from_tree(&tree, &rope);
    let mut parse_diags = Vec::new();
    collect_parse_errors(&tree, &rope, &mut parse_diags);
    Some(DiskFile {
        mtime,
        symbols,
        parse_diags,
    })
}

/// Iterative DFS over `roots`, returning every `*.foobar` file found.
fn walk_foobar_files(roots: &[PathBuf]) -> Vec<PathBuf> {
    const SKIP_DIRS: &[&str] = &["node_modules", "target"];

    let mut out: Vec<PathBuf> = Vec::new();
    let mut visited: HashSet<PathBuf> = HashSet::new();
    let mut stack: Vec<PathBuf> = Vec::new();

    for root in roots {
        let canon = root.canonicalize().unwrap_or_else(|_| root.clone());
        if visited.insert(canon.clone()) {
            stack.push(canon);
        }
    }

    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(OsStr::to_str) else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let Ok(ty) = entry.file_type() else {
                continue;
            };
            if ty.is_dir() {
                if SKIP_DIRS.contains(&name) {
                    continue;
                }
                let canon = path.canonicalize().unwrap_or_else(|_| path.clone());
                if visited.insert(canon.clone()) {
                    stack.push(canon);
                }
            } else if ty.is_file() && path.extension() == Some(OsStr::new("foobar")) {
                out.push(path);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn open_doc_preferred_over_disk() {
        let ws = Workspace::default();
        let dir = TempDir::new().unwrap();
        let inc_path = dir.path().join("inc.foobar");
        let main_path = dir.path().join("main.foobar");
        fs::write(&inc_path, "defines\nname: \"disk\"\n").unwrap();
        fs::write(&main_path, "include inc.foobar\n").unwrap();

        // inc.foobar is open with unsaved edits that differ from disk.
        let inc_uri = path_to_uri(&inc_path).unwrap();
        let inc_doc = Document::from_text(inc_uri, "defines\nname: \"buffer\"\n");
        ws.docs
            .insert(inc_doc.uri.clone(), Arc::new(RwLock::new(inc_doc)));

        let main_doc =
            Document::from_text(path_to_uri(&main_path).unwrap(), "include inc.foobar\n");
        let hit = ws.lookup_define(&main_doc, "name").await.unwrap();
        assert_eq!(hit.entry.value_text, "buffer");
        assert!(matches!(hit.source, DefineSource::OpenDoc(_)));
    }

    #[tokio::test]
    async fn dependents_follow_edits() {
        let ws = Workspace::default();
        let dir = TempDir::new().unwrap();
        let inc_path = dir.path().join("inc.foobar");
        fs::write(&inc_path, "defines\nx: \"1\"\n").unwrap();
        let inc_canon = inc_path.canonicalize().unwrap();
        let main_uri = path_to_uri(&dir.path().join("main.foobar")).unwrap();

        let v1 = Document::from_text(main_uri.clone(), "include inc.foobar\n");
        ws.refresh_includes_for(&v1).await;
        assert_eq!(ws.dependents_of(&inc_canon), vec![main_uri.clone()]);

        // The include is removed: inc.foobar no longer has main as a dependent.
        let v2 = Document::from_text(main_uri, "defines\ny: \"2\"\n");
        ws.refresh_includes_for(&v2).await;
        assert!(ws.dependents_of(&inc_canon).is_empty());
    }

    #[tokio::test]
    async fn has_define_falls_back_to_workspace_pool() {
        let ws = Workspace::default();
        let dir = TempDir::new().unwrap();
        // b.foobar uses %(foo) without including a.foobar, which defines it.
        fs::write(dir.path().join("a.foobar"), "defines\nfoo: \"hello\"\n").unwrap();
        let b = Document::from_text(
            path_to_uri(&dir.path().join("b.foobar")).unwrap(),
            "defines\nbar: \"%(foo) %(ghost)\"\n",
        );

        ws.set_roots(vec![dir.path().to_path_buf()]);
        ws.scan_workspace();

        assert!(ws.has_define(&b.uri, &b.symbols, "foo").await);
        assert!(!ws.has_define(&b.uri, &b.symbols, "ghost").await);
    }

    #[test]
    fn scan_workspace_skips_hidden_and_build_dirs() {
        let ws = Workspace::default();
        let dir = TempDir::new().unwrap();
        for sub in [".git", "node_modules", "target", "nested"] {
            fs::create_dir_all(dir.path().join(sub)).unwrap();
            fs::write(dir.path().join(sub).join("x.foobar"), "").unwrap();
        }

        ws.set_roots(vec![dir.path().to_path_buf()]);
        ws.scan_workspace();

        let nested = dir.path().join("nested/x.foobar").canonicalize().unwrap();
        assert_eq!(ws.disk.len(), 1);
        assert!(ws.disk.contains_key(&nested));
    }
}
