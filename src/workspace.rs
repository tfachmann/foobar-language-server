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
    pub path: PathBuf,
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

/// Workspace symbol information.
///
/// Keeps track of open documents (and it's symbols).
pub struct Workspace {
    /// Files open in the editor (via LSP).
    pub docs: DashMap<Uri, Arc<RwLock<Document>>>,
    /// Cache of files read from disk (not currently open in the editor).
    pub disk: DashMap<PathBuf, Arc<DiskFile>>,
    /// Remebers dependent paths.
    pub dependents: DashMap<PathBuf, HashSet<Uri>>,
    /// Workspace root directories captured at LSP `initialize`.
    pub roots: OnceLock<Vec<PathBuf>>,
}

impl Workspace {
    pub fn new() -> Self {
        Self {
            docs: DashMap::new(),
            disk: DashMap::new(),
            dependents: DashMap::new(),
            roots: OnceLock::new(),
        }
    }

    pub fn set_roots(&self, roots: Vec<PathBuf>) {
        let _ = self.roots.set(roots);
    }

    /// Recursively walk every stored root, parsing each `*.foobar` file into `self.disk`.
    pub fn scan_workspace(&self) {
        let Some(roots) = self.roots.get() else {
            return;
        };
        for path in walk_foobar_files(roots) {
            if self.find_open_doc(&path).is_some() {
                continue;
            }
            let mtime = fs::metadata(&path).and_then(|m| m.modified()).ok();
            let cached_is_fresh = self
                .disk
                .get(&path)
                .map(|e| e.mtime == mtime)
                .unwrap_or(false);
            if cached_is_fresh {
                continue;
            }
            if let Some(loaded) = self.load_disk_file(&path) {
                self.disk.insert(path, Arc::new(loaded));
            }
        }
    }

    /// Resolve a raw include path string against a base `file://` URI.
    ///
    /// Returns `None` for non-file base URIs.
    pub fn resolve(&self, base: &Uri, raw: &str) -> Option<ResolvedInclude> {
        let base_path = uri_to_path(base)?;
        let base_dir = base_path.parent()?;
        let joined = base_dir.join(raw.trim());
        let canonical = joined.canonicalize().ok();
        let final_path = canonical.unwrap_or(joined);
        let exists = final_path.exists();
        Some(ResolvedInclude {
            path: final_path,
            exists,
        })
    }

    /// Find the open-doc URI whose canonical path equals `path`, if any.
    pub fn find_open_doc(&self, path: &Path) -> Option<Uri> {
        for entry in self.docs.iter() {
            let uri = entry.key();
            if let Some(p) = uri_to_path(uri)
                && p.canonicalize().ok().as_deref() == Some(path)
            {
                return Some(uri.clone());
            }
        }
        None
    }

    /// Eagerly load all transitive includes of `doc` into the disk cache.
    ///
    /// Detects cycles by skipping paths that resolve to an open doc.
    pub async fn refresh_includes_for(&self, doc: &Document) {
        let mut visited: HashSet<PathBuf> = HashSet::new();
        let mut queue: VecDeque<(Uri, Vec<IncludeRef>)> = VecDeque::new();
        queue.push_back((doc.uri.clone(), doc.symbols.includes.clone()));

        while let Some((base, includes)) = queue.pop_front() {
            for inc in includes {
                let Some(resolved) = self.resolve(&base, &inc.raw_path) else {
                    continue;
                };
                if !visited.insert(resolved.path.clone()) {
                    continue;
                }
                if !resolved.exists {
                    continue;
                }
                // Open-doc version is authoritative; skip disk load. Recurse via its includes.
                if let Some(open_uri) = self.find_open_doc(&resolved.path) {
                    if let Some(open_doc) = self.docs.get(&open_uri) {
                        let guard = open_doc.read().await;
                        queue.push_back((guard.uri.clone(), guard.symbols.includes.clone()));
                    }
                    continue;
                }
                // Load from disk (or reuse cache if mtime unchanged).
                let mtime = fs::metadata(&resolved.path).and_then(|m| m.modified()).ok();
                let cached_is_fresh = self
                    .disk
                    .get(&resolved.path)
                    .map(|e| e.mtime == mtime)
                    .unwrap_or(false);
                if !cached_is_fresh {
                    if let Some(loaded) = self.load_disk_file(&resolved.path) {
                        self.disk.insert(resolved.path.clone(), Arc::new(loaded));
                    }
                }
                if let Some(file) = self.disk.get(&resolved.path) {
                    let next_base = path_to_uri(&resolved.path).unwrap_or_else(|| base.clone());
                    queue.push_back((next_base, file.symbols.includes.clone()));
                }
            }
        }

        self.rebuild_dependents_for(&doc.uri, &visited);
    }

    fn rebuild_dependents_for(&self, uri: &Uri, included_paths: &HashSet<PathBuf>) {
        for mut entry in self.dependents.iter_mut() {
            entry.value_mut().remove(uri);
        }
        self.dependents.retain(|_, set| !set.is_empty());
        for path in included_paths {
            self.dependents
                .entry(path.clone())
                .or_default()
                .insert(uri.clone());
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

    /// Read + parse + index a file from disk.
    ///
    /// Returns `None` on any IO/parse failure.
    fn load_disk_file(&self, path: &Path) -> Option<DiskFile> {
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
        let mtime = fs::metadata(path).and_then(|m| m.modified()).ok();
        Some(DiskFile {
            path: path.to_path_buf(),
            mtime,
            symbols,
            parse_diags,
        })
    }

    /// Walk the transitive include chain to find a definition for `name`.
    /// Prefers open-doc symbols over disk cache. Cycle-detected.
    pub async fn lookup_define(&self, doc: &Document, name: &str) -> Option<DefineHit> {
        // Local first.
        if let Some(entry) = doc.symbols.defines.get(name) {
            return Some(DefineHit {
                entry: entry.clone(),
                source: DefineSource::OpenDoc(doc.uri.clone()),
            });
        }

        let mut visited: HashSet<PathBuf> = HashSet::new();
        let mut queue: VecDeque<(Uri, Vec<IncludeRef>)> = VecDeque::new();
        queue.push_back((doc.uri.clone(), doc.symbols.includes.clone()));

        while let Some((base, includes)) = queue.pop_front() {
            for inc in includes {
                let Some(resolved) = self.resolve(&base, &inc.raw_path) else {
                    continue;
                };
                if !visited.insert(resolved.path.clone()) {
                    continue;
                }
                if !resolved.exists {
                    continue;
                }

                // Prefer open-doc version.
                if let Some(open_uri) = self.find_open_doc(&resolved.path) {
                    if let Some(open_doc) = self.docs.get(&open_uri) {
                        let guard = open_doc.read().await;
                        if let Some(entry) = guard.symbols.defines.get(name) {
                            return Some(DefineHit {
                                entry: entry.clone(),
                                source: DefineSource::OpenDoc(open_uri),
                            });
                        }
                        queue.push_back((guard.uri.clone(), guard.symbols.includes.clone()));
                        continue;
                    }
                }

                // Disk fallback.
                if let Some(file) = self.disk.get(&resolved.path) {
                    if let Some(entry) = file.symbols.defines.get(name) {
                        return Some(DefineHit {
                            entry: entry.clone(),
                            source: DefineSource::Disk(resolved.path.clone()),
                        });
                    }
                    let next_base = path_to_uri(&resolved.path).unwrap_or_else(|| base.clone());
                    queue.push_back((next_base, file.symbols.includes.clone()));
                }
            }
        }

        None
    }

    /// Cheap variant of [`Self::lookup_define`].
    /// Answers "does `name` resolve from this base?".
    ///
    /// `local` is the perspective file's own symbol index (open or disk).
    pub async fn has_define(&self, base_uri: &Uri, local: &SymbolIndex, name: &str) -> bool {
        if local.defines.contains_key(name) {
            return true;
        }

        let mut visited: HashSet<PathBuf> = HashSet::new();
        let mut queue: VecDeque<(Uri, Vec<IncludeRef>)> = VecDeque::new();
        queue.push_back((base_uri.clone(), local.includes.clone()));

        while let Some((base, includes)) = queue.pop_front() {
            for inc in includes {
                let Some(resolved) = self.resolve(&base, &inc.raw_path) else {
                    continue;
                };
                if !visited.insert(resolved.path.clone()) {
                    continue;
                }
                if !resolved.exists {
                    continue;
                }

                if let Some(open_uri) = self.find_open_doc(&resolved.path) {
                    if let Some(open_doc) = self.docs.get(&open_uri) {
                        let guard = open_doc.read().await;
                        if guard.symbols.defines.contains_key(name) {
                            return true;
                        }
                        queue.push_back((guard.uri.clone(), guard.symbols.includes.clone()));
                        continue;
                    }
                }

                if let Some(file) = self.disk.get(&resolved.path) {
                    if file.symbols.defines.contains_key(name) {
                        return true;
                    }
                    let next_base = path_to_uri(&resolved.path).unwrap_or_else(|| base.clone());
                    queue.push_back((next_base, file.symbols.includes.clone()));
                }
            }
        }

        // Workspace-wide pool fallback.
        let doc_arcs: Vec<Arc<RwLock<Document>>> =
            self.docs.iter().map(|e| e.value().clone()).collect();
        for doc_arc in doc_arcs {
            if doc_arc.read().await.symbols.defines.contains_key(name) {
                return true;
            }
        }
        for entry in self.disk.iter() {
            if entry.value().symbols.defines.contains_key(name) {
                return true;
            }
        }
        false
    }

    /// Workspace-wide count of `%(name)` references.
    pub async fn count_refs(&self, name: &str) -> usize {
        let mut n = 0usize;
        let mut doc_paths: HashSet<PathBuf> = HashSet::new();
        let doc_arcs: Vec<Arc<RwLock<Document>>> =
            self.docs.iter().map(|e| e.value().clone()).collect();
        for doc_arc in doc_arcs {
            let guard = doc_arc.read().await;
            n += guard
                .symbols
                .var_refs
                .iter()
                .filter(|(k, _)| k == name)
                .count();
            if let Some(p) = uri_to_path(&guard.uri).and_then(|p| p.canonicalize().ok()) {
                doc_paths.insert(p);
            }
        }
        for entry in self.disk.iter() {
            if doc_paths.contains(entry.key()) {
                continue;
            }
            n += entry
                .value()
                .symbols
                .var_refs
                .iter()
                .filter(|(k, _)| k == name)
                .count();
        }
        n
    }

    /// Count defines in a resolved include target (looking at open doc first, then disk).
    ///
    /// Returns `None` if the file isn't loaded (neither open nor cached).
    pub async fn defines_count_at(&self, path: &Path) -> Option<usize> {
        if let Some(open_uri) = self.find_open_doc(path) {
            if let Some(open_doc) = self.docs.get(&open_uri) {
                return Some(open_doc.read().await.symbols.defines.len());
            }
        }
        self.disk.get(path).map(|f| f.symbols.defines.len())
    }
}

impl Default for Workspace {
    fn default() -> Self {
        Self::new()
    }
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
    use crate::document::Document;
    use std::str::FromStr;
    use tempfile::TempDir;
    use tower_lsp_server::ls_types::{DidOpenTextDocumentParams, TextDocumentItem};

    fn open_doc(uri: Uri, text: &str) -> Document {
        Document::open(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri,
                language_id: "foobar".to_string(),
                version: 1,
                text: text.to_string(),
            },
        })
        .unwrap()
    }

    #[test]
    fn resolve_joins_relative() {
        let ws = Workspace::new();
        let dir = TempDir::new().unwrap();
        let main_path = dir.path().join("main.foobar");
        let sub_path = dir.path().join("sub.foobar");
        fs::write(&main_path, "").unwrap();
        fs::write(&sub_path, "").unwrap();

        let base = path_to_uri(&main_path).unwrap();
        let resolved = ws.resolve(&base, "sub.foobar").unwrap();
        assert!(resolved.exists);
        assert_eq!(resolved.path, sub_path.canonicalize().unwrap());
    }

    #[test]
    fn resolve_reports_missing() {
        let ws = Workspace::new();
        let dir = TempDir::new().unwrap();
        let main_path = dir.path().join("main.foobar");
        fs::write(&main_path, "").unwrap();
        let base = path_to_uri(&main_path).unwrap();
        let resolved = ws.resolve(&base, "nope.foobar").unwrap();
        assert!(!resolved.exists);
        assert!(resolved.path.ends_with("nope.foobar"));
    }

    #[tokio::test]
    async fn lookup_define_via_include_disk() {
        let ws = Workspace::new();
        let dir = TempDir::new().unwrap();
        let inc_path = dir.path().join("inc.foobar");
        let main_path = dir.path().join("main.foobar");
        fs::write(&inc_path, "defines\nname: \"timo\"\n").unwrap();
        fs::write(
            &main_path,
            "include inc.foobar\n\ndefines\nfoo: \"%(name)\"\n",
        )
        .unwrap();
        let main_uri = path_to_uri(&main_path).unwrap();
        let doc = open_doc(main_uri.clone(), &fs::read_to_string(&main_path).unwrap());

        ws.refresh_includes_for(&doc).await;
        let hit = ws
            .lookup_define(&doc, "name")
            .await
            .expect("should resolve");
        assert_eq!(hit.entry.value_text, "timo");
        match hit.source {
            DefineSource::Disk(p) => assert_eq!(p, inc_path.canonicalize().unwrap()),
            DefineSource::OpenDoc(_) => panic!("expected Disk source"),
        }
    }

    #[tokio::test]
    async fn cycle_detection() {
        let ws = Workspace::new();
        let dir = TempDir::new().unwrap();
        let a_path = dir.path().join("a.foobar");
        let b_path = dir.path().join("b.foobar");
        fs::write(&a_path, "include b.foobar\n").unwrap();
        fs::write(&b_path, "include a.foobar\n").unwrap();
        let a_uri = path_to_uri(&a_path).unwrap();
        let doc = open_doc(a_uri, &fs::read_to_string(&a_path).unwrap());

        // Must terminate (cycle-safe).
        ws.refresh_includes_for(&doc).await;
        // Both files cached, lookup of missing name returns None without hanging.
        assert!(ws.lookup_define(&doc, "nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn open_doc_preferred_over_disk() {
        let ws = Workspace::new();
        let dir = TempDir::new().unwrap();
        let inc_path = dir.path().join("inc.foobar");
        let main_path = dir.path().join("main.foobar");
        fs::write(&inc_path, "defines\nname: \"disk\"\n").unwrap();
        fs::write(&main_path, "include inc.foobar\n").unwrap();

        // main opened with its include
        let main_uri = path_to_uri(&main_path).unwrap();
        let main_doc = open_doc(main_uri.clone(), &fs::read_to_string(&main_path).unwrap());
        ws.docs.insert(
            main_uri.clone(),
            Arc::new(RwLock::new(open_doc(
                main_uri.clone(),
                &fs::read_to_string(&main_path).unwrap(),
            ))),
        );

        // opened with different (edit-buffer) content overriding disk
        let inc_uri = path_to_uri(&inc_path).unwrap();
        ws.docs.insert(
            inc_uri.clone(),
            Arc::new(RwLock::new(open_doc(
                inc_uri.clone(),
                "defines\nname: \"buffer\"\n",
            ))),
        );

        ws.refresh_includes_for(&main_doc).await;
        let hit = ws.lookup_define(&main_doc, "name").await.unwrap();
        assert_eq!(hit.entry.value_text, "buffer");
        assert!(matches!(hit.source, DefineSource::OpenDoc(_)));
    }

    #[tokio::test]
    async fn rebuild_dependents_basic() {
        let ws = Workspace::new();
        let dir = TempDir::new().unwrap();
        let inc_path = dir.path().join("inc.foobar");
        let main_path = dir.path().join("main.foobar");
        fs::write(&inc_path, "defines\nx: \"1\"\n").unwrap();
        let main_text = "include inc.foobar\n";
        fs::write(&main_path, main_text).unwrap();
        let main_uri = path_to_uri(&main_path).unwrap();
        let main_doc = open_doc(main_uri.clone(), main_text);

        ws.refresh_includes_for(&main_doc).await;
        let inc_canon = inc_path.canonicalize().unwrap();
        let deps = ws.dependents_of(&inc_canon);
        assert_eq!(deps, vec![main_uri]);
    }

    #[tokio::test]
    async fn rebuild_dependents_remove_on_reparse() {
        let ws = Workspace::new();
        let dir = TempDir::new().unwrap();
        let inc_path = dir.path().join("inc.foobar");
        let main_path = dir.path().join("main.foobar");
        fs::write(&inc_path, "defines\nx: \"1\"\n").unwrap();
        let main_uri = path_to_uri(&main_path).unwrap();

        // First version includes inc.foobar
        let v1 = open_doc(main_uri.clone(), "include inc.foobar\n");
        ws.refresh_includes_for(&v1).await;
        let inc_canon = inc_path.canonicalize().unwrap();
        assert_eq!(ws.dependents_of(&inc_canon), vec![main_uri.clone()]);

        // Second version drops the include
        let v2 = open_doc(main_uri.clone(), "defines\ny: \"2\"\n");
        ws.refresh_includes_for(&v2).await;
        assert!(ws.dependents_of(&inc_canon).is_empty());
    }

    #[tokio::test]
    async fn forget_doc_clears_entries() {
        let ws = Workspace::new();
        let dir = TempDir::new().unwrap();
        let inc_path = dir.path().join("inc.foobar");
        let main_path = dir.path().join("main.foobar");
        fs::write(&inc_path, "defines\nx: \"1\"\n").unwrap();
        let main_uri = path_to_uri(&main_path).unwrap();
        let doc = open_doc(main_uri.clone(), "include inc.foobar\n");

        ws.refresh_includes_for(&doc).await;
        let inc_canon = inc_path.canonicalize().unwrap();
        assert_eq!(ws.dependents_of(&inc_canon), vec![main_uri.clone()]);

        ws.forget_doc(&main_uri);
        assert!(ws.dependents_of(&inc_canon).is_empty());
    }

    #[test]
    fn scan_workspace_loads_files() {
        let ws = Workspace::new();
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("a.foobar");
        let b = dir.path().join("nested").join("b.foobar");
        fs::create_dir_all(b.parent().unwrap()).unwrap();
        fs::write(&a, "defines\nx: \"1\"\n").unwrap();
        fs::write(&b, "defines\ny: \"2\"\n").unwrap();

        ws.set_roots(vec![dir.path().to_path_buf()]);
        ws.scan_workspace();

        let a_canon = a.canonicalize().unwrap();
        let b_canon = b.canonicalize().unwrap();
        assert!(ws.disk.get(&a_canon).is_some(), "missing a");
        assert!(ws.disk.get(&b_canon).is_some(), "missing b");
    }

    #[test]
    fn scan_workspace_skips_hidden_and_blacklisted_dirs() {
        let ws = Workspace::new();
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        fs::create_dir_all(dir.path().join("node_modules")).unwrap();
        fs::create_dir_all(dir.path().join("target")).unwrap();
        fs::write(dir.path().join(".git/x.foobar"), "").unwrap();
        fs::write(dir.path().join("node_modules/y.foobar"), "").unwrap();
        fs::write(dir.path().join("target/z.foobar"), "").unwrap();
        let c = dir.path().join("c.foobar");
        fs::write(&c, "").unwrap();

        ws.set_roots(vec![dir.path().to_path_buf()]);
        ws.scan_workspace();

        let c_canon = c.canonicalize().unwrap();
        assert!(ws.disk.get(&c_canon).is_some());
        assert_eq!(ws.disk.len(), 1, "skipped dirs leaked in");
    }

    #[test]
    fn scan_workspace_respects_mtime_cache() {
        let ws = Workspace::new();
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("a.foobar");
        fs::write(&a, "defines\nx: \"1\"\n").unwrap();
        ws.set_roots(vec![dir.path().to_path_buf()]);
        ws.scan_workspace();
        let canon = a.canonicalize().unwrap();
        let first = Arc::clone(&ws.disk.get(&canon).unwrap());
        // Second scan with no file change: should not reload (same Arc).
        ws.scan_workspace();
        let second = Arc::clone(&ws.disk.get(&canon).unwrap());
        assert!(Arc::ptr_eq(&first, &second), "reloaded despite same mtime");
    }

    #[tokio::test]
    async fn scan_workspace_skips_open_docs() {
        let ws = Workspace::new();
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("a.foobar");
        fs::write(&a, "defines\nx: \"1\"\n").unwrap();
        let a_uri = path_to_uri(&a).unwrap();
        ws.docs.insert(
            a_uri.clone(),
            Arc::new(RwLock::new(open_doc(a_uri, "defines\nx: \"1\"\n"))),
        );

        ws.set_roots(vec![dir.path().to_path_buf()]);
        ws.scan_workspace();

        let canon = a.canonicalize().unwrap();
        assert!(
            ws.disk.get(&canon).is_none(),
            "open-doc file leaked into disk cache"
        );
    }

    #[test]
    fn scan_workspace_handles_missing_root() {
        let ws = Workspace::new();
        ws.set_roots(vec![PathBuf::from("/this/path/does/not/exist")]);
        ws.scan_workspace(); // must not panic
        assert_eq!(ws.disk.len(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn scan_workspace_symlink_loop_terminates() {
        let ws = Workspace::new();
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("a.foobar");
        fs::write(&a, "").unwrap();
        std::os::unix::fs::symlink(dir.path(), dir.path().join("self_loop")).unwrap();

        ws.set_roots(vec![dir.path().to_path_buf()]);
        ws.scan_workspace(); // must terminate
        let canon = a.canonicalize().unwrap();
        assert!(ws.disk.get(&canon).is_some());
    }

    #[tokio::test]
    async fn has_define_pool_fallback_finds_unrelated_file() {
        let ws = Workspace::new();
        let dir = TempDir::new().unwrap();
        // A defines foo. B does NOT include A and uses %(foo).
        let a = dir.path().join("a.foobar");
        let b = dir.path().join("b.foobar");
        fs::write(&a, "defines\nfoo: \"hello\"\n").unwrap();
        fs::write(&b, "defines\nbar: \"%(foo)\"\n").unwrap();

        ws.set_roots(vec![dir.path().to_path_buf()]);
        ws.scan_workspace();

        let b_uri = path_to_uri(&b).unwrap();
        let b_doc = open_doc(b_uri.clone(), &fs::read_to_string(&b).unwrap());
        // Pool fallback must see foo from a.foobar even though b doesn't include a.
        assert!(ws.has_define(&b_uri, &b_doc.symbols, "foo").await);
    }

    #[tokio::test]
    async fn has_define_pool_fallback_false_when_nowhere() {
        let ws = Workspace::new();
        let dir = TempDir::new().unwrap();
        let b = dir.path().join("b.foobar");
        fs::write(&b, "defines\nbar: \"%(ghost)\"\n").unwrap();

        ws.set_roots(vec![dir.path().to_path_buf()]);
        ws.scan_workspace();

        let b_uri = path_to_uri(&b).unwrap();
        let b_doc = open_doc(b_uri.clone(), &fs::read_to_string(&b).unwrap());
        assert!(!ws.has_define(&b_uri, &b_doc.symbols, "ghost").await);
    }

    #[tokio::test]
    async fn has_define_local_still_wins() {
        // Regression: pool fallback must not change the local-hit path.
        let ws = Workspace::new();
        let uri = Uri::from_str("file:///x.foobar").unwrap();
        let doc = open_doc(uri.clone(), "defines\nfoo: \"1\"\n");
        assert!(ws.has_define(&uri, &doc.symbols, "foo").await);
    }
}
