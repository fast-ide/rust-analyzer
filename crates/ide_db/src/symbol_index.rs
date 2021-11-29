//! This module handles fuzzy-searching of functions, structs and other symbols
//! by name across the whole workspace and dependencies.
//!
//! It works by building an incrementally-updated text-search index of all
//! symbols. The backbone of the index is the **awesome** `fst` crate by
//! @BurntSushi.
//!
//! In a nutshell, you give a set of strings to `fst`, and it builds a
//! finite state machine describing this set of strings. The strings which
//! could fuzzy-match a pattern can also be described by a finite state machine.
//! What is freaking cool is that you can now traverse both state machines in
//! lock-step to enumerate the strings which are both in the input set and
//! fuzz-match the query. Or, more formally, given two languages described by
//! FSTs, one can build a product FST which describes the intersection of the
//! languages.
//!
//! `fst` does not support cheap updating of the index, but it supports unioning
//! of state machines. So, to account for changing source code, we build an FST
//! for each library (which is assumed to never change) and an FST for each Rust
//! file in the current workspace, and run a query against the union of all
//! those FSTs.

use std::{
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
    mem,
    sync::Arc,
};

use base_db::{
    salsa::{self, ParallelDatabase},
    CrateId, FileId, FileRange, SourceDatabaseExt, SourceRootId, Upcast,
};
use either::Either;
use fst::{self, Streamer};
use hir::{
    db::{DefDatabase, HirDatabase},
    AdtId, AssocContainerId, AssocItemId, AssocItemLoc, DefHasSource, DefWithBodyId, HasSource,
    HirFileId, ImplId, InFile, ItemLoc, ItemTreeNode, Lookup, MacroDef, ModuleDefId, ModuleId,
    Semantics, TraitId,
};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use syntax::{
    ast::{self, HasName},
    AstNode, Parse, SmolStr, SourceFile, SyntaxNode, SyntaxNodePtr,
};

use crate::RootDatabase;

#[derive(Debug)]
pub struct Query {
    query: String,
    lowercased: String,
    only_types: bool,
    libs: bool,
    exact: bool,
    case_sensitive: bool,
    limit: usize,
}

impl Query {
    pub fn new(query: String) -> Query {
        let lowercased = query.to_lowercase();
        Query {
            query,
            lowercased,
            only_types: false,
            libs: false,
            exact: false,
            case_sensitive: false,
            limit: usize::max_value(),
        }
    }

    pub fn only_types(&mut self) {
        self.only_types = true;
    }

    pub fn libs(&mut self) {
        self.libs = true;
    }

    pub fn exact(&mut self) {
        self.exact = true;
    }

    pub fn case_sensitive(&mut self) {
        self.case_sensitive = true;
    }

    pub fn limit(&mut self, limit: usize) {
        self.limit = limit
    }
}

#[salsa::query_group(SymbolsDatabaseStorage)]
pub trait SymbolsDatabase: HirDatabase + SourceDatabaseExt + Upcast<dyn HirDatabase> {
    fn module_symbols(&self, module_id: ModuleId) -> Arc<SymbolIndex>;
    fn library_symbols(&self) -> Arc<FxHashMap<SourceRootId, SymbolIndex>>;
    /// The set of "local" (that is, from the current workspace) roots.
    /// Files in local roots are assumed to change frequently.
    #[salsa::input]
    fn local_roots(&self) -> Arc<FxHashSet<SourceRootId>>;
    /// The set of roots for crates.io libraries.
    /// Files in libraries are assumed to never change.
    #[salsa::input]
    fn library_roots(&self) -> Arc<FxHashSet<SourceRootId>>;
}

fn library_symbols(db: &dyn SymbolsDatabase) -> Arc<FxHashMap<SourceRootId, SymbolIndex>> {
    let _p = profile::span("library_symbols");

    let roots = db.library_roots();
    let res = roots
        .iter()
        .map(|&root_id| {
            let root = db.source_root(root_id);
            let files = root
                .iter()
                .map(|it| (it, SourceDatabaseExt::file_text(db, it)))
                .collect::<Vec<_>>();
            let symbol_index = SymbolIndex::for_files(
                files.into_par_iter().map(|(file, text)| (file, SourceFile::parse(&text))),
            );
            (root_id, symbol_index)
        })
        .collect();
    Arc::new(res)
}

fn module_symbols(db: &dyn SymbolsDatabase, module_id: ModuleId) -> Arc<SymbolIndex> {
    let symbols = SymbolCollector::collect(db, module_id);
    Arc::new(SymbolIndex::new(symbols))
}

/// Need to wrap Snapshot to provide `Clone` impl for `map_with`
struct Snap<DB>(DB);
impl<DB: ParallelDatabase> Clone for Snap<salsa::Snapshot<DB>> {
    fn clone(&self) -> Snap<salsa::Snapshot<DB>> {
        Snap(self.0.snapshot())
    }
}

// Feature: Workspace Symbol
//
// Uses fuzzy-search to find types, modules and functions by name across your
// project and dependencies. This is **the** most useful feature, which improves code
// navigation tremendously. It mostly works on top of the built-in LSP
// functionality, however `#` and `*` symbols can be used to narrow down the
// search. Specifically,
//
// - `Foo` searches for `Foo` type in the current workspace
// - `foo#` searches for `foo` function in the current workspace
// - `Foo*` searches for `Foo` type among dependencies, including `stdlib`
// - `foo#*` searches for `foo` function among dependencies
//
// That is, `#` switches from "types" to all symbols, `*` switches from the current
// workspace to dependencies.
//
// Note that filtering does not currently work in VSCode due to the editor never
// sending the special symbols to the language server. Instead, you can configure
// the filtering via the `rust-analyzer.workspace.symbol.search.scope` and
// `rust-analyzer.workspace.symbol.search.kind` settings.
//
// |===
// | Editor  | Shortcut
//
// | VS Code | kbd:[Ctrl+T]
// |===
pub fn world_symbols(db: &RootDatabase, query: Query) -> Vec<FileSymbol> {
    let _p = profile::span("world_symbols").detail(|| query.query.clone());

    let tmp1;
    let tmp2;
    let buf: Vec<&SymbolIndex> = if query.libs {
        tmp1 = db.library_symbols();
        tmp1.values().collect()
    } else {
        let mut module_ids = Vec::new();

        for &root in db.local_roots().iter() {
            let crates = db.source_root_crates(root);
            for &krate in crates.iter() {
                module_ids.extend(module_ids_for_crate(db, krate));
            }
        }

        let snap = Snap(db.snapshot());
        tmp2 = module_ids
            .par_iter()
            .map_with(snap, |snap, &module_id| snap.0.module_symbols(module_id))
            .collect::<Vec<_>>();
        tmp2.iter().map(|it| &**it).collect()
    };
    query.search(&buf)
}

pub fn crate_symbols(db: &RootDatabase, krate: CrateId, query: Query) -> Vec<FileSymbol> {
    let _p = profile::span("crate_symbols").detail(|| format!("{:?}", query));

    let module_ids = module_ids_for_crate(db, krate);
    let snap = Snap(db.snapshot());
    let buf: Vec<_> = module_ids
        .par_iter()
        .map_with(snap, |snap, &module_id| snap.0.module_symbols(module_id))
        .collect();

    let buf = buf.iter().map(|it| &**it).collect::<Vec<_>>();
    query.search(&buf)
}

fn module_ids_for_crate(db: &RootDatabase, krate: CrateId) -> Vec<ModuleId> {
    let def_map = db.crate_def_map(krate);
    def_map.modules().map(|(id, _)| def_map.module_id(id)).collect()
}

pub fn index_resolve(db: &RootDatabase, name: &str) -> Vec<FileSymbol> {
    let mut query = Query::new(name.to_string());
    query.exact();
    query.limit(4);
    world_symbols(db, query)
}

#[derive(Default)]
pub struct SymbolIndex {
    symbols: Vec<FileSymbol>,
    map: fst::Map<Vec<u8>>,
}

impl fmt::Debug for SymbolIndex {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("SymbolIndex").field("n_symbols", &self.symbols.len()).finish()
    }
}

impl PartialEq for SymbolIndex {
    fn eq(&self, other: &SymbolIndex) -> bool {
        self.symbols == other.symbols
    }
}

impl Eq for SymbolIndex {}

impl Hash for SymbolIndex {
    fn hash<H: Hasher>(&self, hasher: &mut H) {
        self.symbols.hash(hasher)
    }
}

impl SymbolIndex {
    fn new(mut symbols: Vec<FileSymbol>) -> SymbolIndex {
        fn cmp(lhs: &FileSymbol, rhs: &FileSymbol) -> Ordering {
            let lhs_chars = lhs.name.chars().map(|c| c.to_ascii_lowercase());
            let rhs_chars = rhs.name.chars().map(|c| c.to_ascii_lowercase());
            lhs_chars.cmp(rhs_chars)
        }

        symbols.par_sort_by(cmp);

        let mut builder = fst::MapBuilder::memory();

        let mut last_batch_start = 0;

        for idx in 0..symbols.len() {
            if let Some(next_symbol) = symbols.get(idx + 1) {
                if cmp(&symbols[last_batch_start], next_symbol) == Ordering::Equal {
                    continue;
                }
            }

            let start = last_batch_start;
            let end = idx + 1;
            last_batch_start = end;

            let key = symbols[start].name.as_str().to_ascii_lowercase();
            let value = SymbolIndex::range_to_map_value(start, end);

            builder.insert(key, value).unwrap();
        }

        let map = fst::Map::new(builder.into_inner().unwrap()).unwrap();
        SymbolIndex { symbols, map }
    }

    pub fn len(&self) -> usize {
        self.symbols.len()
    }

    pub fn memory_size(&self) -> usize {
        self.map.as_fst().size() + self.symbols.len() * mem::size_of::<FileSymbol>()
    }

    pub(crate) fn for_files(
        files: impl ParallelIterator<Item = (FileId, Parse<ast::SourceFile>)>,
    ) -> SymbolIndex {
        let symbols = files
            .flat_map(|(file_id, file)| source_file_to_file_symbols(&file.tree(), file_id))
            .collect::<Vec<_>>();
        SymbolIndex::new(symbols)
    }

    fn range_to_map_value(start: usize, end: usize) -> u64 {
        debug_assert![start <= (std::u32::MAX as usize)];
        debug_assert![end <= (std::u32::MAX as usize)];

        ((start as u64) << 32) | end as u64
    }

    fn map_value_to_range(value: u64) -> (usize, usize) {
        let end = value as u32 as usize;
        let start = (value >> 32) as usize;
        (start, end)
    }
}

impl Query {
    pub(crate) fn search(self, indices: &[&SymbolIndex]) -> Vec<FileSymbol> {
        let _p = profile::span("symbol_index::Query::search");
        let mut op = fst::map::OpBuilder::new();
        for file_symbols in indices.iter() {
            let automaton = fst::automaton::Subsequence::new(&self.lowercased);
            op = op.add(file_symbols.map.search(automaton))
        }
        let mut stream = op.union();
        let mut res = Vec::new();
        while let Some((_, indexed_values)) = stream.next() {
            for indexed_value in indexed_values {
                let symbol_index = &indices[indexed_value.index];
                let (start, end) = SymbolIndex::map_value_to_range(indexed_value.value);

                for symbol in &symbol_index.symbols[start..end] {
                    if self.only_types && !symbol.kind.is_type() {
                        continue;
                    }
                    if self.exact {
                        if symbol.name != self.query {
                            continue;
                        }
                    } else if self.case_sensitive {
                        if self.query.chars().any(|c| !symbol.name.contains(c)) {
                            continue;
                        }
                    }

                    res.push(symbol.clone());
                    if res.len() >= self.limit {
                        return res;
                    }
                }
            }
        }
        res
    }
}

/// The actual data that is stored in the index. It should be as compact as
/// possible.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileSymbol {
    pub name: SmolStr,
    pub loc: DeclarationLocation,
    pub kind: FileSymbolKind,
    pub container_name: Option<SmolStr>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeclarationLocation {
    /// The file id for both the `ptr` and `name_ptr`.
    pub hir_file_id: HirFileId,
    /// This points to the whole syntax node of the declaration.
    pub ptr: SyntaxNodePtr,
    /// This points to the [`syntax::ast::Name`] identifier of the declaration.
    pub name_ptr: SyntaxNodePtr,
}

impl DeclarationLocation {
    pub fn syntax(&self, semantics: &Semantics<'_, RootDatabase>) -> Option<SyntaxNode> {
        let root = semantics.parse_or_expand(self.hir_file_id)?;
        Some(self.ptr.to_node(&root))
    }

    pub fn original_range(&self, semantics: &Semantics<'_, RootDatabase>) -> Option<FileRange> {
        find_original_file_range(semantics, self.hir_file_id, &self.ptr)
    }

    pub fn original_name_range(
        &self,
        semantics: &Semantics<'_, RootDatabase>,
    ) -> Option<FileRange> {
        find_original_file_range(semantics, self.hir_file_id, &self.name_ptr)
    }
}

fn find_original_file_range(
    semantics: &Semantics<'_, RootDatabase>,
    file_id: HirFileId,
    ptr: &SyntaxNodePtr,
) -> Option<FileRange> {
    let root = semantics.parse_or_expand(file_id)?;
    let node = ptr.to_node(&root);
    let node = InFile::new(file_id, &node);

    Some(node.original_file_range(semantics.db.upcast()))
}

#[derive(PartialEq, Eq, Hash, Clone, Copy, Debug)]
pub enum FileSymbolKind {
    Const,
    Enum,
    Function,
    Macro,
    Module,
    Static,
    Struct,
    Trait,
    TypeAlias,
    Union,
}

impl FileSymbolKind {
    fn is_type(self: FileSymbolKind) -> bool {
        matches!(
            self,
            FileSymbolKind::Struct
                | FileSymbolKind::Enum
                | FileSymbolKind::Trait
                | FileSymbolKind::TypeAlias
                | FileSymbolKind::Union
        )
    }
}

fn source_file_to_file_symbols(_source_file: &SourceFile, _file_id: FileId) -> Vec<FileSymbol> {
    // todo: delete this.
    vec![]
}
enum SymbolCollectorWork {
    Module { module_id: ModuleId, parent: Option<DefWithBodyId> },
    Body { body_id: DefWithBodyId },
    Impl { impl_id: ImplId },
    Trait { trait_id: TraitId },
}

struct SymbolCollector<'a> {
    db: &'a dyn SymbolsDatabase,
    symbols: Vec<FileSymbol>,
    work: Vec<SymbolCollectorWork>,
    container_name_stack: Vec<SmolStr>,
}

/// Given a [`ModuleId`] and a [`SymbolsDatabase`], use the DefMap for the module's crate to collect all symbols that should be
/// indexed for the given module.
impl<'a> SymbolCollector<'a> {
    fn collect(db: &dyn SymbolsDatabase, module_id: ModuleId) -> Vec<FileSymbol> {
        let mut symbol_collector = SymbolCollector {
            db,
            symbols: Default::default(),
            container_name_stack: Default::default(),
            work: vec![SymbolCollectorWork::Module { module_id, parent: None }],
        };

        while let Some(work) = symbol_collector.work.pop() {
            symbol_collector.do_work(work);
        }

        symbol_collector.symbols
    }

    fn do_work(&mut self, work: SymbolCollectorWork) {
        self.db.unwind_if_cancelled();

        match work {
            SymbolCollectorWork::Module { module_id, parent } => {
                let parent_name = parent.and_then(|id| self.def_with_body_id_name(id));
                self.with_container_name(parent_name, |s| s.collect_from_module(module_id));
            }
            SymbolCollectorWork::Trait { trait_id } => {
                let trait_name = self.db.trait_data(trait_id).name.as_text();
                self.with_container_name(trait_name, |s| s.collect_from_trait(trait_id));
            }
            SymbolCollectorWork::Body { body_id } => self.collect_from_body(body_id),
            SymbolCollectorWork::Impl { impl_id } => self.collect_from_impl(impl_id),
        }
    }

    fn collect_from_module(&mut self, module_id: ModuleId) {
        let def_map = module_id.def_map(self.db.upcast());
        let module_data = &def_map[module_id.local_id];
        let scope = &module_data.scope;

        for module_def_id in scope.declarations() {
            match module_def_id {
                ModuleDefId::ModuleId(id) => self.push_module(id),
                ModuleDefId::FunctionId(id) => {
                    self.push_decl_assoc(id, FileSymbolKind::Function);
                    self.work.push(SymbolCollectorWork::Body { body_id: id.into() });
                }
                ModuleDefId::AdtId(AdtId::StructId(id)) => {
                    self.push_decl(id, FileSymbolKind::Struct)
                }
                ModuleDefId::AdtId(AdtId::EnumId(id)) => self.push_decl(id, FileSymbolKind::Enum),
                ModuleDefId::AdtId(AdtId::UnionId(id)) => self.push_decl(id, FileSymbolKind::Union),
                ModuleDefId::ConstId(id) => {
                    self.push_decl_assoc(id, FileSymbolKind::Const);
                    self.work.push(SymbolCollectorWork::Body { body_id: id.into() })
                }
                ModuleDefId::StaticId(id) => {
                    self.push_decl(id, FileSymbolKind::Static);
                    self.work.push(SymbolCollectorWork::Body { body_id: id.into() })
                }
                ModuleDefId::TraitId(id) => {
                    self.push_decl(id, FileSymbolKind::Trait);
                    self.work.push(SymbolCollectorWork::Trait { trait_id: id })
                }
                ModuleDefId::TypeAliasId(id) => {
                    self.push_decl_assoc(id, FileSymbolKind::TypeAlias);
                }
                // Don't index these.
                ModuleDefId::BuiltinType(_) => {}
                ModuleDefId::EnumVariantId(_) => {}
            }
        }

        for impl_id in scope.impls() {
            self.work.push(SymbolCollectorWork::Impl { impl_id });
        }

        for const_id in scope.unnamed_consts() {
            self.work.push(SymbolCollectorWork::Body { body_id: const_id.into() })
        }

        for macro_def_id in scope.macro_declarations() {
            self.push_decl_macro(macro_def_id.into());
        }
    }

    fn collect_from_body(&mut self, body_id: DefWithBodyId) {
        let body = self.db.body(body_id);

        // Descend into the blocks and enqueue collection of all modules within.
        for (_, def_map) in body.blocks(self.db.upcast()) {
            for (id, _) in def_map.modules() {
                self.work.push(SymbolCollectorWork::Module {
                    module_id: def_map.module_id(id),
                    parent: Some(body_id),
                });
            }
        }
    }

    fn collect_from_impl(&mut self, impl_id: ImplId) {
        let impl_data = self.db.impl_data(impl_id);
        for &assoc_item_id in &impl_data.items {
            self.push_assoc_item(assoc_item_id)
        }
    }

    fn collect_from_trait(&mut self, trait_id: TraitId) {
        let trait_data = self.db.trait_data(trait_id);
        for &(_, assoc_item_id) in &trait_data.items {
            self.push_assoc_item(assoc_item_id);
        }
    }

    fn with_container_name(&mut self, container_name: Option<SmolStr>, f: impl FnOnce(&mut Self)) {
        if let Some(container_name) = container_name {
            self.container_name_stack.push(container_name);
            f(self);
            self.container_name_stack.pop();
        } else {
            f(self);
        }
    }

    fn current_container_name(&self) -> Option<SmolStr> {
        self.container_name_stack.last().cloned()
    }

    fn def_with_body_id_name(&self, body_id: DefWithBodyId) -> Option<SmolStr> {
        match body_id {
            DefWithBodyId::FunctionId(id) => Some(
                id.lookup(self.db.upcast()).source(self.db.upcast()).value.name()?.text().into(),
            ),
            DefWithBodyId::StaticId(id) => Some(
                id.lookup(self.db.upcast()).source(self.db.upcast()).value.name()?.text().into(),
            ),
            DefWithBodyId::ConstId(id) => Some(
                id.lookup(self.db.upcast()).source(self.db.upcast()).value.name()?.text().into(),
            ),
        }
    }

    fn push_assoc_item(&mut self, assoc_item_id: AssocItemId) {
        match assoc_item_id {
            AssocItemId::FunctionId(id) => self.push_decl_assoc(id, FileSymbolKind::Function),
            AssocItemId::ConstId(id) => self.push_decl_assoc(id, FileSymbolKind::Const),
            AssocItemId::TypeAliasId(id) => self.push_decl_assoc(id, FileSymbolKind::TypeAlias),
        }
    }

    fn push_decl_assoc<L, T>(&mut self, id: L, kind: FileSymbolKind)
    where
        L: Lookup<Data = AssocItemLoc<T>>,
        T: ItemTreeNode,
        <T as ItemTreeNode>::Source: HasName,
    {
        fn container_name(db: &dyn DefDatabase, container: AssocContainerId) -> Option<SmolStr> {
            match container {
                AssocContainerId::ModuleId(module_id) => {
                    let def_map = module_id.def_map(db);
                    let module_data = &def_map[module_id.local_id];
                    module_data
                        .origin
                        .declaration()
                        .and_then(|s| s.to_node(db.upcast()).name().map(|n| n.text().into()))
                }
                AssocContainerId::TraitId(trait_id) => {
                    let loc = trait_id.lookup(db);
                    let source = loc.source(db);
                    source.value.name().map(|n| n.text().into())
                }
                AssocContainerId::ImplId(_) => None,
            }
        }

        self.push_file_symbol(|s| {
            let loc = id.lookup(s.db.upcast());
            let source = loc.source(s.db.upcast());
            let name_node = source.value.name()?;
            let container_name =
                container_name(s.db.upcast(), loc.container).or_else(|| s.current_container_name());

            Some(FileSymbol {
                name: name_node.text().into(),
                kind,
                container_name,
                loc: DeclarationLocation {
                    hir_file_id: source.file_id,
                    ptr: SyntaxNodePtr::new(source.value.syntax()),
                    name_ptr: SyntaxNodePtr::new(name_node.syntax()),
                },
            })
        })
    }

    fn push_decl<L, T>(&mut self, id: L, kind: FileSymbolKind)
    where
        L: Lookup<Data = ItemLoc<T>>,
        T: ItemTreeNode,
        <T as ItemTreeNode>::Source: HasName,
    {
        self.push_file_symbol(|s| {
            let loc = id.lookup(s.db.upcast());
            let source = loc.source(s.db.upcast());
            let name_node = source.value.name()?;

            Some(FileSymbol {
                name: name_node.text().into(),
                kind,
                container_name: s.current_container_name(),
                loc: DeclarationLocation {
                    hir_file_id: source.file_id,
                    ptr: SyntaxNodePtr::new(source.value.syntax()),
                    name_ptr: SyntaxNodePtr::new(name_node.syntax()),
                },
            })
        })
    }

    fn push_module(&mut self, module_id: ModuleId) {
        self.push_file_symbol(|s| {
            let def_map = module_id.def_map(s.db.upcast());
            let module_data = &def_map[module_id.local_id];
            let declaration = module_data.origin.declaration()?;
            let module = declaration.to_node(s.db.upcast());
            let name_node = module.name()?;

            Some(FileSymbol {
                name: name_node.text().into(),
                kind: FileSymbolKind::Module,
                container_name: s.current_container_name(),
                loc: DeclarationLocation {
                    hir_file_id: declaration.file_id,
                    ptr: SyntaxNodePtr::new(module.syntax()),
                    name_ptr: SyntaxNodePtr::new(name_node.syntax()),
                },
            })
        })
    }

    fn push_decl_macro(&mut self, macro_def: MacroDef) {
        self.push_file_symbol(|s| {
            let name = macro_def.name(s.db.upcast())?.as_text()?;
            let source = macro_def.source(s.db.upcast())?;

            let (ptr, name_ptr) = match source.value {
                Either::Left(m) => {
                    (SyntaxNodePtr::new(m.syntax()), SyntaxNodePtr::new(m.name()?.syntax()))
                }
                Either::Right(f) => {
                    (SyntaxNodePtr::new(f.syntax()), SyntaxNodePtr::new(f.name()?.syntax()))
                }
            };

            Some(FileSymbol {
                name,
                kind: FileSymbolKind::Macro,
                container_name: s.current_container_name(),
                loc: DeclarationLocation { hir_file_id: source.file_id, name_ptr, ptr },
            })
        })
    }

    fn push_file_symbol(&mut self, f: impl FnOnce(&Self) -> Option<FileSymbol>) {
        if let Some(file_symbol) = f(self) {
            self.symbols.push(file_symbol);
        }
    }
}
