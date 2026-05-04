//! Rails-aware recognition layered on top of the Ruby extractor.
//!
//! Recognizes the most common Rails DSL patterns and emits synthetic
//! symbols + edges so the graph reflects relationships that would
//! otherwise be invisible (because Rails defines them at runtime via
//! `define_method`):
//!
//! * `has_many` / `has_one` / `belongs_to` / `has_and_belongs_to_many`
//!   → synthetic `Property` symbol + `REFERENCES` edge to the inferred
//!     model class (singularized + class-cased), or the `class_name:`
//!     override when present
//! * `attr_accessor` / `attr_reader` / `attr_writer`
//!   → synthetic `Property` symbol per name
//! * `before_action` / `after_create` / `before_save` / etc.
//!   → `CALLS` edge from the enclosing class to the named method
//! * `scope :name, -> { ... }`
//!   → synthetic class `Method` symbol
//! * `include Mixin` / `extend Mixin` / `prepend Mixin`
//!   → `IMPLEMENTS` edge to the named module
//!
//! Recognition runs unconditionally — no "is this a Rails app?" check.
//! Outside Rails these patterns are rare enough that false positives
//! are negligible.

use ast_graph_core::*;
use crate::extractor::*;
use inflector::Inflector;
use std::path::Path;

/// Try to recognize a Rails DSL pattern in a `call` node sitting at
/// class-body level. Returns `true` if the call was consumed (so the
/// caller can skip its other fallbacks for this node).
pub fn recognize_rails_pattern(
    source: &[u8],
    call_node: &tree_sitter::Node,
    file_path: &Path,
    container_id: NodeId,
    container_qualified: &str,
    visibility: Visibility,
    symbols: &mut Vec<SymbolNode>,
    raw_edges: &mut Vec<RawEdge>,
) -> bool {
    if call_node.kind() != "call" {
        return false;
    }
    // Bareword DSL calls have no explicit receiver.
    if child_by_field(call_node, "receiver").is_some() {
        return false;
    }
    let method_node = match child_by_field(call_node, "method") {
        Some(n) => n,
        None => return false,
    };
    let method_name = node_text(source, &method_node);

    let args = collect_args(source, call_node);

    match method_name {
        "has_many" | "has_one" | "belongs_to" | "has_and_belongs_to_many" => {
            handle_association(
                method_name, &args, source, call_node, file_path,
                container_id, container_qualified, symbols, raw_edges,
            );
            true
        }
        "attr_accessor" | "attr_reader" | "attr_writer" => {
            handle_attr(
                method_name, &args, call_node, file_path,
                container_id, container_qualified, visibility, symbols,
            );
            true
        }
        "scope" => {
            handle_scope(&args, call_node, file_path, container_id, container_qualified, symbols);
            true
        }
        "include" | "extend" | "prepend" => {
            handle_mixin(&args, call_node, container_id, raw_edges);
            true
        }
        m if is_rails_callback(m) => {
            handle_callback(&args, call_node, container_id, container_qualified, raw_edges);
            true
        }
        "validates" | "validate" => {
            handle_validates(
                &args, call_node, container_id, container_qualified, raw_edges,
            );
            true
        }
        "delegate" => {
            handle_delegate(
                &args, call_node, file_path, container_id, container_qualified, symbols,
            );
            true
        }
        "enum" => {
            handle_enum(
                source, call_node, file_path, container_id, container_qualified, symbols,
            );
            true
        }
        "helper_method" => {
            // ActionController: `helper_method :foo` exposes `foo` to views.
            // Emit a CALLS edge so the action method isn't flagged as dead.
            handle_callback(&args, call_node, container_id, container_qualified, raw_edges);
            true
        }
        "identified_by" => {
            // ActionCable channel identifier — synthetic Property.
            handle_attr(
                "identified_by", &args, call_node, file_path,
                container_id, container_qualified, visibility, symbols,
            );
            true
        }
        _ => false,
    }
}

/// `validates :name, :email, presence: true` — emit REFERENCES edges from
/// the class to each named attribute. The attributes themselves come from
/// the database schema (invisible) or `attr_accessor`; the edge documents
/// the class-level dependency either way.
fn handle_validates(
    args: &[Arg],
    call: &tree_sitter::Node,
    container_id: NodeId,
    container_qualified: &str,
    raw_edges: &mut Vec<RawEdge>,
) {
    let line = call.start_position().row as u32;
    for arg in args {
        let attr_name = match arg {
            Arg::Symbol(s) => s.clone(),
            _ => continue,
        };
        raw_edges.push(RawEdge {
            source: container_id,
            kind: EdgeKind::References,
            target_name: format!("{container_qualified}.{attr_name}"),
            target_module: None,
            source_line: line,
        });
    }
}

/// `delegate :name, :email, to: :user` — emit synthetic Method symbols for
/// each delegated name so callers of `instance.name` resolve through this
/// class (otherwise they fall to name-only resolution).  No REFERENCES
/// edge to the target is emitted — `:user` is a method/property name, not
/// a class, and we can't infer the actual class statically.
fn handle_delegate(
    args: &[Arg],
    call: &tree_sitter::Node,
    file_path: &Path,
    container_id: NodeId,
    container_qualified: &str,
    symbols: &mut Vec<SymbolNode>,
) {
    // Skip if no `to:` kwarg — `delegate` without `to:` is invalid Rails.
    let has_to = args.iter().any(|a| matches!(a, Arg::Pair(k, _) if k == "to"));
    if !has_to {
        return;
    }
    let line = call.start_position().row as u32;
    for arg in args {
        let name = match arg {
            Arg::Symbol(s) => s.clone(),
            _ => continue,
        };
        let qualified = format!("{container_qualified}.{name}");
        let id = NodeId::new(
            &file_path.to_string_lossy(),
            &qualified,
            SymbolKind::Method,
            line,
        );
        symbols.push(SymbolNode {
            id,
            name: qualified,
            kind: SymbolKind::Method,
            file_path: file_path.to_path_buf(),
            line_range: (line, call.end_position().row as u32),
            signature: Some(format!("delegate :{name}")),
            doc_comment: None,
            visibility: Visibility::Public,
            language: Language::Ruby,
            parent: Some(container_id),
        });
    }
}

/// `enum status: { active: 0, inactive: 1 }` (Rails 6 form) or
/// `enum :status, { active: 0, ... }` / `enum :status, [:active, :inactive]`
/// (Rails 7 forms).  For each enum value, emit synthetic Method symbols
/// `Class.value?` and `Class.value!` so calls to those generated methods
/// resolve and don't show up as dead code.
fn handle_enum(
    source: &[u8],
    call: &tree_sitter::Node,
    file_path: &Path,
    container_id: NodeId,
    container_qualified: &str,
    symbols: &mut Vec<SymbolNode>,
) {
    // Walk the call's arguments directly to find enum values.  Cases:
    //   1. `enum status: { active: 0, inactive: 1 }`   — pair, key=field, value=hash
    //   2. `enum :status, { active: 0, inactive: 1 }`  — symbol then hash
    //   3. `enum :status, [:active, :inactive]`        — symbol then array
    //   4. `enum :status, %i[active inactive]`         — symbol then array (string-array)
    let args_node = match child_by_field(call, "arguments") {
        Some(a) => a,
        None => return,
    };

    let mut values: Vec<String> = Vec::new();
    let mut cursor = args_node.walk();
    for arg in args_node.children(&mut cursor) {
        if !arg.is_named() {
            continue;
        }
        match arg.kind() {
            // Form 1: `status: { active: 0, ... }` — pair where value is a hash
            "pair" => {
                let mut c = arg.walk();
                let value_node = arg.children(&mut c)
                    .filter(|n| n.is_named())
                    .nth(1);
                if let Some(v) = value_node {
                    extract_enum_values_from(source, &v, &mut values);
                }
            }
            // Forms 2–4: positional hash, array, or symbol/string array (`%i[]`/`%w[]`)
            "hash" | "array" | "symbol_array" | "string_array" => {
                extract_enum_values_from(source, &arg, &mut values);
            }
            _ => {}
        }
    }

    let line = call.start_position().row as u32;
    for value in values {
        // Skip if the name has anything that can't be a valid method suffix.
        if value.is_empty() || !value.chars().all(|c| c.is_alphanumeric() || c == '_') {
            continue;
        }
        for suffix in ['?', '!'] {
            let qualified = format!("{container_qualified}.{value}{suffix}");
            let id = NodeId::new(
                &file_path.to_string_lossy(),
                &qualified,
                SymbolKind::Method,
                line,
            );
            symbols.push(SymbolNode {
                id,
                name: qualified,
                kind: SymbolKind::Method,
                file_path: file_path.to_path_buf(),
                line_range: (line, call.end_position().row as u32),
                signature: Some(format!("enum #{value}{suffix}")),
                doc_comment: None,
                visibility: Visibility::Public,
                language: Language::Ruby,
                parent: Some(container_id),
            });
        }
    }
}

/// Walk a hash or array node collecting enum value names.
fn extract_enum_values_from(source: &[u8], node: &tree_sitter::Node, out: &mut Vec<String>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() {
            continue;
        }
        match child.kind() {
            // Hash entry: take the key name.
            "pair" => {
                let mut c = child.walk();
                let key = child.children(&mut c).find(|n| n.is_named());
                if let Some(key) = key {
                    let raw = node_text(source, &key);
                    let bare = raw.trim_end_matches(':').trim_start_matches(':').to_string();
                    out.push(bare);
                }
            }
            // Array element: a symbol literal like `:active` or a string in `%w[]` / `%i[]`.
            "simple_symbol" | "bare_symbol" => {
                let raw = node_text(source, &child);
                out.push(raw.trim_start_matches(':').to_string());
            }
            "string" | "bare_string" => {
                out.push(extract_string_literal(source, &child));
            }
            "string_array" | "symbol_array" => {
                // %w[active inactive] / %i[active inactive] — children are
                // bare_string / bare_symbol literals.
                let mut c2 = child.walk();
                for w in child.children(&mut c2) {
                    if w.is_named() {
                        out.push(node_text(source, &w).trim_start_matches(':').to_string());
                    }
                }
            }
            _ => {}
        }
    }
}

/// Singularize an association name. Wraps `Inflector::to_singular()` and
/// fills in irregulars that the upstream crate's special-cases list
/// misses (most notably `people → person`, `children → child`).
///
/// Trailing-suffix-only check — won't mis-fire on compound names like
/// `corporate_people` (still returns `corporate_person`).
fn singularize(word: &str) -> String {
    // (plural_suffix, singular_suffix) — applied case-sensitively to
    // a lowercase view of the word, then re-cased onto the original.
    const IRREGULARS: &[(&str, &str)] = &[
        ("people", "person"),
        ("children", "child"),
        ("mice", "mouse"),
        ("lice", "louse"),
        ("alumni", "alumnus"),
        ("cacti", "cactus"),
        ("foci", "focus"),
        ("fungi", "fungus"),
        ("nuclei", "nucleus"),
        ("syllabi", "syllabus"),
        ("radii", "radius"),
        ("phenomena", "phenomenon"),
        ("criteria", "criterion"),
    ];
    let lower = word.to_lowercase();
    for (pl, sg) in IRREGULARS {
        if lower == *pl {
            return sg.to_string();
        }
        // Compound form: `corporate_people` → `corporate_person`.
        // Match on `_<plural>` boundary to avoid `triple` → `tripson`.
        let needle = format!("_{pl}");
        if lower.ends_with(&needle) {
            let prefix = &word[..word.len() - pl.len()];
            return format!("{prefix}{sg}");
        }
    }
    word.to_singular()
}

/// `before_action`, `after_save`, etc. Used to filter callbacks from
/// arbitrary Ruby calls. Includes `skip_*` variants (still produce a
/// CALLS edge — the relationship to the named method exists either way).
fn is_rails_callback(name: &str) -> bool {
    // Rails AR/AC lifecycle callbacks. Kept explicit (rather than a
    // `before_*` glob) to avoid false positives on user methods.
    const CALLBACKS: &[&str] = &[
        "before_action", "after_action", "around_action",
        "skip_before_action", "skip_after_action", "skip_around_action",
        "before_filter", "after_filter", "around_filter",  // Rails 4 legacy
        "skip_before_filter", "skip_after_filter",
        "before_save", "after_save", "around_save",
        "before_create", "after_create", "around_create",
        "before_update", "after_update", "around_update",
        "before_destroy", "after_destroy", "around_destroy",
        "before_validation", "after_validation",
        "before_commit", "after_commit", "after_rollback",
        "after_initialize", "after_find", "after_touch",
    ];
    CALLBACKS.contains(&name)
}

/// One argument extracted from an `argument_list`.  No `Str` variant —
/// no current handler consumes top-level string args, so they fall
/// through to `Other` with identical behavior.
#[derive(Debug, Clone)]
enum Arg {
    /// `:foo` — leading colon stripped.
    Symbol(String),
    /// `Foo` — bare constant reference.
    Constant(String),
    /// `key: value` or `:key => value`. The value has its quotes/colons
    /// stripped if it's a string/symbol.
    Pair(String, String),
    /// Anything else (string literal, lambda, hash, nested call, …) —
    /// captured so callers can decide how to handle.
    Other,
}

fn collect_args(source: &[u8], call: &tree_sitter::Node) -> Vec<Arg> {
    let args_node = match child_by_field(call, "arguments") {
        Some(a) => a,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    let mut cursor = args_node.walk();
    for arg in args_node.children(&mut cursor) {
        if !arg.is_named() {
            continue;
        }
        out.push(parse_arg(source, &arg));
    }
    out
}

fn parse_arg(source: &[u8], node: &tree_sitter::Node) -> Arg {
    match node.kind() {
        "simple_symbol" => {
            // Text is `:foo` — strip leading colon.
            let raw = node_text(source, node);
            Arg::Symbol(raw.trim_start_matches(':').to_string())
        }
        "string" => Arg::Other,
        "constant" => Arg::Constant(node_text(source, node).to_string()),
        "scope_resolution" => {
            // Foo::Bar in arg position — treat the whole text as a constant ref.
            Arg::Constant(node_text(source, node).to_string())
        }
        "pair" => {
            // Pair has key + value as field-less children. tree-sitter-ruby
            // exposes them as the first two named children.
            let mut c = node.walk();
            let mut named = node.children(&mut c).filter(|n| n.is_named());
            let key_node = match named.next() {
                Some(n) => n,
                None => return Arg::Other,
            };
            let value_node = match named.next() {
                Some(n) => n,
                None => return Arg::Other,
            };
            let key = node_text(source, &key_node)
                // hash_key_symbol prints as `name:` — strip trailing colon.
                .trim_end_matches(':')
                .trim_start_matches(':')
                .to_string();
            let value = match value_node.kind() {
                "string" => extract_string_literal(source, &value_node),
                "simple_symbol" => node_text(source, &value_node).trim_start_matches(':').to_string(),
                "constant" | "scope_resolution" => node_text(source, &value_node).to_string(),
                _ => node_text(source, &value_node).to_string(),
            };
            Arg::Pair(key, value)
        }
        _ => Arg::Other,
    }
}

/// Strip surrounding quotes from a tree-sitter `string` node's text.
/// Tree-sitter-ruby wraps the literal in delimiter tokens; for the
/// common single-segment case the inner text is what we want.
fn extract_string_literal(source: &[u8], node: &tree_sitter::Node) -> String {
    // Find the inner string_content if present; fall back to whole text
    // with quotes stripped.
    if let Some(inner) = find_child_by_kind(node, "string_content") {
        return node_text(source, &inner).to_string();
    }
    let raw = node_text(source, node);
    raw.trim_matches(|c| c == '"' || c == '\'').to_string()
}

// ── handlers ────────────────────────────────────────────────────────────────

fn handle_association(
    method_name: &str,
    args: &[Arg],
    source: &[u8],
    call: &tree_sitter::Node,
    file_path: &Path,
    container_id: NodeId,
    container_qualified: &str,
    symbols: &mut Vec<SymbolNode>,
    raw_edges: &mut Vec<RawEdge>,
) {
    let _ = source;
    // First arg is the association name as a symbol.
    let assoc_name = match args.first() {
        Some(Arg::Symbol(s)) => s.clone(),
        _ => return,
    };

    // `class_name:` override wins; otherwise infer from the symbol via
    // singularize → class_case.  `has_many :posts` → "Post"; `belongs_to
    // :user` → "User"; `has_many :user_posts` → "UserPost".
    let class_name = args.iter().find_map(|a| match a {
        Arg::Pair(k, v) if k == "class_name" => Some(v.clone()),
        _ => None,
    }).unwrap_or_else(|| {
        // belongs_to/has_one are singular; has_many/HABTM are plural.
        // Singularize unconditionally — no-op when already singular.
        singularize(&assoc_name).to_class_case()
    });

    let qualified = format!("{container_qualified}.{assoc_name}");
    let line = call.start_position().row as u32;
    let id = NodeId::new(
        &file_path.to_string_lossy(),
        &qualified,
        SymbolKind::Property,
        line,
    );

    symbols.push(SymbolNode {
        id,
        name: qualified,
        kind: SymbolKind::Property,
        file_path: file_path.to_path_buf(),
        line_range: (line, call.end_position().row as u32),
        signature: Some(format!("{method_name} :{assoc_name}")),
        doc_comment: None,
        visibility: Visibility::Public,
        language: Language::Ruby,
        parent: Some(container_id),
    });

    raw_edges.push(RawEdge {
        source: id,
        kind: EdgeKind::References,
        target_name: class_name,
        target_module: None,
        source_line: line,
    });
}

fn handle_attr(
    method_name: &str,
    args: &[Arg],
    call: &tree_sitter::Node,
    file_path: &Path,
    container_id: NodeId,
    container_qualified: &str,
    visibility: Visibility,
    symbols: &mut Vec<SymbolNode>,
) {
    let line = call.start_position().row as u32;
    for arg in args {
        let name = match arg {
            Arg::Symbol(s) => s.clone(),
            _ => continue,
        };
        let qualified = format!("{container_qualified}.{name}");
        let id = NodeId::new(
            &file_path.to_string_lossy(),
            &qualified,
            SymbolKind::Property,
            line,
        );
        symbols.push(SymbolNode {
            id,
            name: qualified,
            kind: SymbolKind::Property,
            file_path: file_path.to_path_buf(),
            line_range: (line, call.end_position().row as u32),
            signature: Some(format!("{method_name} :{name}")),
            doc_comment: None,
            visibility,
            language: Language::Ruby,
            parent: Some(container_id),
        });
    }
}

fn handle_callback(
    args: &[Arg],
    call: &tree_sitter::Node,
    container_id: NodeId,
    container_qualified: &str,
    raw_edges: &mut Vec<RawEdge>,
) {
    let line = call.start_position().row as u32;
    // All symbol args are method names. Non-symbol args (`if:`, `only:`,
    // a lambda, an inline block) are skipped — we only emit edges for
    // names we can resolve.
    for arg in args {
        let method_name = match arg {
            Arg::Symbol(s) => s.clone(),
            _ => continue,
        };
        // Qualify to the enclosing class so the resolver matches the
        // intended definition rather than every method by that name.
        let target = format!("{container_qualified}.{method_name}");
        raw_edges.push(RawEdge {
            source: container_id,
            kind: EdgeKind::Calls,
            target_name: target,
            target_module: None,
            source_line: line,
        });
    }
}

fn handle_scope(
    args: &[Arg],
    call: &tree_sitter::Node,
    file_path: &Path,
    container_id: NodeId,
    container_qualified: &str,
    symbols: &mut Vec<SymbolNode>,
) {
    // First arg must be a symbol — the scope name.  The body lambda is
    // captured by the runtime; we just need the class method symbol.
    let scope_name = match args.first() {
        Some(Arg::Symbol(s)) => s.clone(),
        _ => return,
    };
    let qualified = format!("{container_qualified}.{scope_name}");
    let line = call.start_position().row as u32;
    let id = NodeId::new(
        &file_path.to_string_lossy(),
        &qualified,
        SymbolKind::Method,
        line,
    );
    symbols.push(SymbolNode {
        id,
        name: qualified,
        kind: SymbolKind::Method,
        file_path: file_path.to_path_buf(),
        line_range: (line, call.end_position().row as u32),
        signature: Some(format!("scope :{scope_name}")),
        doc_comment: None,
        visibility: Visibility::Public,
        language: Language::Ruby,
        parent: Some(container_id),
    });
}

fn handle_mixin(
    args: &[Arg],
    call: &tree_sitter::Node,
    container_id: NodeId,
    raw_edges: &mut Vec<RawEdge>,
) {
    let line = call.start_position().row as u32;
    for arg in args {
        let mixin = match arg {
            Arg::Constant(c) => c.clone(),
            _ => continue,
        };
        raw_edges.push(RawEdge {
            source: container_id,
            kind: EdgeKind::Implements,
            target_name: mixin,
            target_module: None,
            source_line: line,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extractor::LanguageExtractor;
    use crate::lang::ruby::RubyExtractor;

    fn extract(src: &str) -> (Vec<SymbolNode>, Vec<RawEdge>) {
        let extractor = RubyExtractor;
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&extractor.tree_sitter_language()).unwrap();
        let tree = parser.parse(src.as_bytes(), None).unwrap();
        let r = extractor.extract(src.as_bytes(), &tree, Path::new("test.rb"));
        (r.symbols, r.raw_edges)
    }

    fn find<'a>(syms: &'a [SymbolNode], name: &str) -> Option<&'a SymbolNode> {
        syms.iter().find(|s| s.name == name)
    }

    fn refs_from<'a>(edges: &'a [RawEdge], src: NodeId) -> Vec<&'a str> {
        edges.iter()
            .filter(|e| e.source == src && e.kind == EdgeKind::References)
            .map(|e| e.target_name.as_str())
            .collect()
    }

    fn calls_from<'a>(edges: &'a [RawEdge], src: NodeId) -> Vec<&'a str> {
        edges.iter()
            .filter(|e| e.source == src && e.kind == EdgeKind::Calls)
            .map(|e| e.target_name.as_str())
            .collect()
    }

    fn impls_from<'a>(edges: &'a [RawEdge], src: NodeId) -> Vec<&'a str> {
        edges.iter()
            .filter(|e| e.source == src && e.kind == EdgeKind::Implements)
            .map(|e| e.target_name.as_str())
            .collect()
    }

    // ── associations ─────────────────────────────────────────────────────────

    #[test]
    fn has_many_emits_property_and_references() {
        let src = "class User\n  has_many :posts\nend\n";
        let (syms, edges) = extract(src);
        let prop = find(&syms, "User.posts").expect("User.posts missing");
        assert_eq!(prop.kind, SymbolKind::Property);

        let refs = refs_from(&edges, prop.id);
        assert!(refs.contains(&"Post"), "expected REFERENCES Post, got: {refs:?}");
    }

    #[test]
    fn belongs_to_singular_already() {
        let src = "class Post\n  belongs_to :user\nend\n";
        let (syms, edges) = extract(src);
        let prop = find(&syms, "Post.user").expect("Post.user missing");
        let refs = refs_from(&edges, prop.id);
        assert!(refs.contains(&"User"));
    }

    #[test]
    fn has_one_singular() {
        let src = "class User\n  has_one :profile\nend\n";
        let (syms, edges) = extract(src);
        let prop = find(&syms, "User.profile").expect("User.profile missing");
        let refs = refs_from(&edges, prop.id);
        assert!(refs.contains(&"Profile"));
    }

    #[test]
    fn habtm_pluralized_name() {
        let src = "class Article\n  has_and_belongs_to_many :categories\nend\n";
        let (syms, edges) = extract(src);
        let prop = find(&syms, "Article.categories").expect("Article.categories missing");
        let refs = refs_from(&edges, prop.id);
        // categories → Category (irregular plural handled by inflector)
        assert!(refs.contains(&"Category"), "got: {refs:?}");
    }

    #[test]
    fn class_name_override_wins() {
        let src = "class User\n  has_many :posts, class_name: 'Article'\nend\n";
        let (syms, edges) = extract(src);
        let prop = find(&syms, "User.posts").unwrap();
        let refs = refs_from(&edges, prop.id);
        assert!(refs.contains(&"Article"), "got: {refs:?}");
        assert!(!refs.contains(&"Post"), "should not also infer Post when class_name given");
    }

    #[test]
    fn irregular_plural_people_to_person() {
        // Tests the inflector dependency: "people" → "person" → "Person"
        // — minimal heuristics would miss this.
        let src = "class Org\n  has_many :people\nend\n";
        let (syms, edges) = extract(src);
        let prop = find(&syms, "Org.people").unwrap();
        let refs = refs_from(&edges, prop.id);
        assert!(refs.contains(&"Person"), "expected Person from inflector, got: {refs:?}");
    }

    #[test]
    fn compound_association_name() {
        let src = "class User\n  has_many :user_posts\nend\n";
        let (syms, edges) = extract(src);
        let prop = find(&syms, "User.user_posts").unwrap();
        let refs = refs_from(&edges, prop.id);
        assert!(refs.contains(&"UserPost"), "got: {refs:?}");
    }

    // ── attr_* ───────────────────────────────────────────────────────────────

    #[test]
    fn attr_accessor_emits_property_per_symbol() {
        let src = "class Foo\n  attr_accessor :name, :email\nend\n";
        let (syms, _) = extract(src);
        assert_eq!(find(&syms, "Foo.name").unwrap().kind, SymbolKind::Property);
        assert_eq!(find(&syms, "Foo.email").unwrap().kind, SymbolKind::Property);
    }

    #[test]
    fn attr_reader_respects_visibility_toggle() {
        // attr_reader after `private` should be Private.
        let src = "class Foo\n  private\n  attr_reader :hidden\nend\n";
        let (syms, _) = extract(src);
        let p = find(&syms, "Foo.hidden").expect("Foo.hidden missing");
        assert_eq!(p.visibility, Visibility::Private);
    }

    // ── callbacks ────────────────────────────────────────────────────────────

    #[test]
    fn before_action_emits_calls_edge_from_class() {
        let src = "class UsersController\n  before_action :authenticate!\nend\n";
        let (syms, edges) = extract(src);
        let cls = find(&syms, "UsersController").unwrap();
        let calls = calls_from(&edges, cls.id);
        assert!(
            calls.contains(&"UsersController.authenticate!"),
            "expected qualified callback edge, got: {calls:?}"
        );
    }

    #[test]
    fn before_action_with_multiple_methods() {
        let src = "class UsersController\n  before_action :authenticate, :authorize\nend\n";
        let (syms, edges) = extract(src);
        let cls = find(&syms, "UsersController").unwrap();
        let calls = calls_from(&edges, cls.id);
        assert!(calls.contains(&"UsersController.authenticate"));
        assert!(calls.contains(&"UsersController.authorize"));
    }

    #[test]
    fn after_save_callback_recognized() {
        let src = "class Post\n  after_save :reindex\nend\n";
        let (syms, edges) = extract(src);
        let cls = find(&syms, "Post").unwrap();
        let calls = calls_from(&edges, cls.id);
        assert!(calls.contains(&"Post.reindex"));
    }

    #[test]
    fn callback_with_kwargs_only_emits_for_symbol_names() {
        // `before_action :auth, only: [:create]` — `only:` is a kwarg, not a method name.
        let src = "class UsersController\n  before_action :auth, only: [:create]\nend\n";
        let (syms, edges) = extract(src);
        let cls = find(&syms, "UsersController").unwrap();
        let calls = calls_from(&edges, cls.id);
        assert!(calls.contains(&"UsersController.auth"));
        // The :create symbol is inside an array kwarg value, not a top-level arg —
        // shouldn't show up as a callback target.
        assert!(!calls.contains(&"UsersController.create"));
    }

    // ── scope ────────────────────────────────────────────────────────────────

    #[test]
    fn scope_emits_class_method() {
        let src = "class Post\n  scope :published, -> { where(published: true) }\nend\n";
        let (syms, _) = extract(src);
        let m = find(&syms, "Post.published").expect("Post.published missing");
        assert_eq!(m.kind, SymbolKind::Method);
    }

    // ── include / extend / prepend ──────────────────────────────────────────

    #[test]
    fn include_emits_implements_edge() {
        let src = "class Foo\n  include Searchable\nend\n";
        let (syms, edges) = extract(src);
        let foo = find(&syms, "Foo").unwrap();
        let impls = impls_from(&edges, foo.id);
        assert!(impls.contains(&"Searchable"), "got: {impls:?}");
    }

    #[test]
    fn extend_emits_implements_edge() {
        let src = "class Foo\n  extend Helpers\nend\n";
        let (syms, edges) = extract(src);
        let foo = find(&syms, "Foo").unwrap();
        let impls = impls_from(&edges, foo.id);
        assert!(impls.contains(&"Helpers"));
    }

    #[test]
    fn include_multiple_modules() {
        let src = "class Foo\n  include Searchable, Cacheable\nend\n";
        let (syms, edges) = extract(src);
        let foo = find(&syms, "Foo").unwrap();
        let impls = impls_from(&edges, foo.id);
        assert!(impls.contains(&"Searchable"));
        assert!(impls.contains(&"Cacheable"));
    }

    // ── validates ────────────────────────────────────────────────────────────

    #[test]
    fn validates_emits_references_per_symbol() {
        let src = "class User\n  validates :email, :name, presence: true\nend\n";
        let (syms, edges) = extract(src);
        let user = find(&syms, "User").unwrap();
        let refs = refs_from(&edges, user.id);
        assert!(refs.contains(&"User.email"), "got: {refs:?}");
        assert!(refs.contains(&"User.name"), "got: {refs:?}");
    }

    #[test]
    fn validate_with_method_name_emits_callback_style_reference() {
        // `validate :method_name` is the custom-validator form.
        let src = "class User\n  validate :complex_check\nend\n";
        let (syms, edges) = extract(src);
        let user = find(&syms, "User").unwrap();
        let refs = refs_from(&edges, user.id);
        assert!(refs.contains(&"User.complex_check"));
    }

    // ── delegate ─────────────────────────────────────────────────────────────

    #[test]
    fn delegate_emits_synthetic_methods() {
        let src = "class User\n  delegate :name, :email, to: :profile\nend\n";
        let (syms, _) = extract(src);
        assert_eq!(find(&syms, "User.name").unwrap().kind, SymbolKind::Method);
        assert_eq!(find(&syms, "User.email").unwrap().kind, SymbolKind::Method);
    }

    #[test]
    fn delegate_without_to_kwarg_skipped() {
        // No `to:` → not a Rails delegate; ignore.
        let src = "class User\n  delegate :something\nend\n";
        let (syms, _) = extract(src);
        assert!(find(&syms, "User.something").is_none());
    }

    // ── enum ─────────────────────────────────────────────────────────────────

    #[test]
    fn enum_kwarg_form_with_hash_emits_predicates_and_bangs() {
        let src = "class Post\n  enum status: { draft: 0, published: 1 }\nend\n";
        let (syms, _) = extract(src);
        assert!(find(&syms, "Post.draft?").is_some(), "Post.draft? missing");
        assert!(find(&syms, "Post.draft!").is_some(), "Post.draft! missing");
        assert!(find(&syms, "Post.published?").is_some());
        assert!(find(&syms, "Post.published!").is_some());
    }

    #[test]
    fn enum_positional_form_with_hash() {
        let src = "class Post\n  enum :status, { draft: 0, published: 1 }\nend\n";
        let (syms, _) = extract(src);
        assert!(find(&syms, "Post.draft?").is_some());
        assert!(find(&syms, "Post.published!").is_some());
    }

    #[test]
    fn enum_positional_form_with_symbol_array() {
        let src = "class Post\n  enum :status, [:draft, :published]\nend\n";
        let (syms, _) = extract(src);
        assert!(find(&syms, "Post.draft?").is_some(), "expected Post.draft?");
        assert!(find(&syms, "Post.published?").is_some());
    }

    #[test]
    fn enum_pct_i_array_form() {
        let src = "class Post\n  enum :status, %i[draft published]\nend\n";
        let (syms, _) = extract(src);
        assert!(find(&syms, "Post.draft?").is_some(), "expected Post.draft? from %i[]");
    }

    // ── helper_method / identified_by ────────────────────────────────────────

    #[test]
    fn helper_method_emits_calls_edge_from_class() {
        let src = "class ApplicationController\n  helper_method :current_user\nend\n";
        let (syms, edges) = extract(src);
        let cls = find(&syms, "ApplicationController").unwrap();
        let calls = calls_from(&edges, cls.id);
        assert!(calls.contains(&"ApplicationController.current_user"), "got: {calls:?}");
    }

    #[test]
    fn identified_by_emits_property() {
        let src = "class ConnectionChannel\n  identified_by :current_user\nend\n";
        let (syms, _) = extract(src);
        let p = find(&syms, "ConnectionChannel.current_user").expect("missing");
        assert_eq!(p.kind, SymbolKind::Property);
    }

    // ── full controller + model smoke ────────────────────────────────────────

    #[test]
    fn controller_smoke_test() {
        let src = r#"class UsersController < ApplicationController
  before_action :authenticate_user!
  before_action :find_user, only: [:show, :edit, :update]

  def index
    @users = User.all
  end

  def show
  end

  private

  def authenticate_user!
    redirect_to login_path unless current_user
  end

  def find_user
    @user = User.find(params[:id])
  end
end
"#;
        let (syms, edges) = extract(src);
        let cls = find(&syms, "UsersController").unwrap();

        // EXTENDS edge from Ruby extractor (not Rails)
        assert!(edges.iter().any(|e|
            e.source == cls.id && e.kind == EdgeKind::Extends && e.target_name == "ApplicationController"
        ));

        // Callback CALLS edges from Rails recognition
        let calls = calls_from(&edges, cls.id);
        assert!(calls.contains(&"UsersController.authenticate_user!"));
        assert!(calls.contains(&"UsersController.find_user"));

        // Visibility tracker still works for actions defined after `private`
        assert_eq!(
            find(&syms, "UsersController.authenticate_user!").unwrap().visibility,
            Visibility::Private
        );
        assert_eq!(
            find(&syms, "UsersController.index").unwrap().visibility,
            Visibility::Public
        );
    }

    #[test]
    fn model_smoke_test() {
        // Use r##"..."## so the Ruby `"#{...}"` interpolation in `full_name`
        // doesn't end the Rust raw string at its first `"#`.
        let src = r##"class User < ApplicationRecord
  include Searchable

  has_many :posts
  has_many :comments, class_name: 'Comment'
  belongs_to :organization
  has_one :profile

  attr_accessor :temporary_password

  scope :active, -> { where(active: true) }

  before_save :normalize_email
  after_create :send_welcome_email

  def full_name
    "#{first_name} #{last_name}"
  end

  private

  def normalize_email
    self.email = email.downcase
  end

  def send_welcome_email
    Mailer.welcome(self).deliver_now
  end
end
"##;
        let (syms, edges) = extract(src);
        let user = find(&syms, "User").unwrap();

        // associations: synthetic Property + REFERENCES
        let posts = find(&syms, "User.posts").expect("User.posts missing");
        assert_eq!(posts.kind, SymbolKind::Property);
        assert!(refs_from(&edges, posts.id).contains(&"Post"));

        let comments = find(&syms, "User.comments").unwrap();
        assert!(refs_from(&edges, comments.id).contains(&"Comment"));

        let org = find(&syms, "User.organization").unwrap();
        assert!(refs_from(&edges, org.id).contains(&"Organization"));

        let profile = find(&syms, "User.profile").unwrap();
        assert!(refs_from(&edges, profile.id).contains(&"Profile"));

        // attr_accessor → Property
        assert_eq!(find(&syms, "User.temporary_password").unwrap().kind, SymbolKind::Property);

        // scope → class Method
        assert_eq!(find(&syms, "User.active").unwrap().kind, SymbolKind::Method);

        // include → IMPLEMENTS
        assert!(impls_from(&edges, user.id).contains(&"Searchable"));

        // callbacks → CALLS edges from class
        let calls = calls_from(&edges, user.id);
        assert!(calls.contains(&"User.normalize_email"));
        assert!(calls.contains(&"User.send_welcome_email"));

        // private toggle still applies to plain `def`s
        assert_eq!(find(&syms, "User.normalize_email").unwrap().visibility, Visibility::Private);
        assert_eq!(find(&syms, "User.full_name").unwrap().visibility, Visibility::Public);
    }
}
