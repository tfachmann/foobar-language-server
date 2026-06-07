//! Shared workspace state
//! - open documents
//! - disk cache of included files
//!
//! Handles transitive symbol lookup with cycle detection.

use std::collections::{HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
}

impl Workspace {
    pub fn new() -> Self {
        Self {
            docs: DashMap::new(),
            disk: DashMap::new(),
            dependents: DashMap::new(),
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

        false
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::Document;
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
}
