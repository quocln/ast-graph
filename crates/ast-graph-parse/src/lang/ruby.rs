use ast_graph_core::*;
use crate::extractor::*;
use std::collections::HashMap;
use std::path::Path;

pub struct RubyExtractor;

impl LanguageExtractor for RubyExtractor {
    fn language(&self) -> Language {
        Language::Ruby
    }

    fn file_extensions(&self) -> &[&str] {
        &["rb"]
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_ruby::LANGUAGE.into()
    }

    fn extract(&self, source: &[u8], tree: &tree_sitter::Tree, file_path: &Path) -> ExtractResult {
        let mut symbols = Vec::new();
        let mut raw_edges = Vec::new();
        let file_str = file_path.to_string_lossy();

        let file_node_id = NodeId::new(&file_str, &file_str, SymbolKind::File, 0);
        symbols.push(SymbolNode {
            id: file_node_id,
            name: file_path.file_name().unwrap_or_default().to_string_lossy().to_string(),
            kind: SymbolKind::File,
            file_path: file_path.to_path_buf(),
            line_range: (0, source.iter().filter(|&&b| b == b'\n').count() as u32),
            signature: None,
            doc_comment: None,
            visibility: Visibility::Public,
            language: Language::Ruby,
            parent: None,
        });

        walk_top(source, &tree.root_node(), file_path, file_node_id, &mut symbols, &mut raw_edges);
        ExtractResult { symbols, raw_edges }
    }
}

/// Walk top-level statements (program or any non-class/module scope).
fn walk_top(
    source: &[u8],
    node: &tree_sitter::Node,
    file_path: &Path,
    parent_id: NodeId,
    symbols: &mut Vec<SymbolNode>,
    raw_edges: &mut Vec<RawEdge>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "module" => extract_module(source, &child, file_path, parent_id, symbols, raw_edges),
            "class" => extract_class(source, &child, file_path, parent_id, symbols, raw_edges),
            "method" => {
                // Top-level def — treated as a function.
                extract_instance_method(
                    source, &child, file_path, parent_id, None, Visibility::Public,
                    symbols, raw_edges,
                );
            }
            "singleton_method" => {
                extract_singleton_method(source, &child, file_path, parent_id, None, symbols, raw_edges);
            }
            "assignment" => {
                extract_constant_assignment(source, &child, file_path, parent_id, None, symbols);
            }
            "call" => {
                // Top-level call — try Rails routes.draw recognition first
                // (which only fires on `<...>routes.draw do ... end`),
                // then fall through to require / require_relative / load.
                let consumed = super::ruby_rails_routes::try_recognize_routes_draw(
                    source, &child, file_path, parent_id, symbols, raw_edges,
                );
                if !consumed {
                    extract_require_call(source, &child, file_path, parent_id, symbols, raw_edges);
                }
            }
            _ => {}
        }
    }
}

/// Extract a `module Name ... end` block.
fn extract_module(
    source: &[u8],
    node: &tree_sitter::Node,
    file_path: &Path,
    parent_id: NodeId,
    symbols: &mut Vec<SymbolNode>,
    raw_edges: &mut Vec<RawEdge>,
) {
    let name_node = match child_by_field(node, "name") {
        Some(n) => n,
        None => return,
    };
    let name = node_text(source, &name_node).to_string();

    let id = NodeId::new(
        &file_path.to_string_lossy(),
        &name,
        SymbolKind::Module,
        node.start_position().row as u32,
    );

    symbols.push(SymbolNode {
        id,
        name: name.clone(),
        kind: SymbolKind::Module,
        file_path: file_path.to_path_buf(),
        line_range: (node.start_position().row as u32, node.end_position().row as u32),
        signature: Some(format!("module {name}")),
        doc_comment: extract_doc_comment_anchor(source, node),
        visibility: Visibility::Public,
        language: Language::Ruby,
        parent: Some(parent_id),
    });

    if let Some(body) = child_by_field(node, "body") {
        walk_class_or_module_body(source, &body, file_path, id, &name, symbols, raw_edges);
    }
}

/// Extract a `class Name [< Super] ... end` block.
fn extract_class(
    source: &[u8],
    node: &tree_sitter::Node,
    file_path: &Path,
    parent_id: NodeId,
    symbols: &mut Vec<SymbolNode>,
    raw_edges: &mut Vec<RawEdge>,
) {
    let name_node = match child_by_field(node, "name") {
        Some(n) => n,
        None => return,
    };
    let name = node_text(source, &name_node).to_string();

    let id = NodeId::new(
        &file_path.to_string_lossy(),
        &name,
        SymbolKind::Class,
        node.start_position().row as u32,
    );

    let superclass_text = child_by_field(node, "superclass").and_then(|sc| {
        // `superclass` wraps the parent name as a child node.
        let mut c = sc.walk();
        let found = sc.children(&mut c).find(|n| n.is_named()).map(|n| node_text(source, &n).to_string());
        found
    });

    let signature = match &superclass_text {
        Some(parent) => format!("class {name} < {parent}"),
        None => format!("class {name}"),
    };

    symbols.push(SymbolNode {
        id,
        name: name.clone(),
        kind: SymbolKind::Class,
        file_path: file_path.to_path_buf(),
        line_range: (node.start_position().row as u32, node.end_position().row as u32),
        signature: Some(signature),
        doc_comment: extract_doc_comment_anchor(source, node),
        visibility: Visibility::Public,
        language: Language::Ruby,
        parent: Some(parent_id),
    });

    if let Some(parent) = superclass_text {
        raw_edges.push(RawEdge {
            source: id,
            kind: EdgeKind::Extends,
            target_name: parent,
            target_module: None,
            source_line: node.start_position().row as u32,
        });
    }

    if let Some(body) = child_by_field(node, "body") {
        walk_class_or_module_body(source, &body, file_path, id, &name, symbols, raw_edges);
    }
}

/// Walk a class/module body with full Ruby visibility tracking:
///   - bareword `private` / `protected` / `public` toggles state for subsequent defs
///   - `private :foo, :bar` (named-arg form) marks those specific methods only
/// Singleton methods (`def self.x`) are always public regardless of toggle state.
fn walk_class_or_module_body(
    source: &[u8],
    body: &tree_sitter::Node,
    file_path: &Path,
    parent_id: NodeId,
    container_name: &str,
    symbols: &mut Vec<SymbolNode>,
    raw_edges: &mut Vec<RawEdge>,
) {
    let mut current_visibility = Visibility::Public;
    // Override map: method-name -> visibility (set by `private :foo` form).
    // Applied after method extraction so it wins over the toggle state.
    let mut targeted_overrides: HashMap<String, Visibility> = HashMap::new();
    // Track inserted method symbol indices by name, so targeted overrides
    // can rewrite their visibility once we hit them.
    let mut method_indices: HashMap<String, Vec<usize>> = HashMap::new();

    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        match child.kind() {
            "method" => {
                let idx = symbols.len();
                let method_name = extract_instance_method(
                    source, &child, file_path, parent_id, Some(container_name),
                    current_visibility, symbols, raw_edges,
                );
                if let Some(n) = method_name {
                    method_indices.entry(n).or_default().push(idx);
                }
            }
            "singleton_method" => {
                extract_singleton_method(
                    source, &child, file_path, parent_id, Some(container_name),
                    symbols, raw_edges,
                );
            }
            "class" => extract_class(source, &child, file_path, parent_id, symbols, raw_edges),
            "module" => extract_module(source, &child, file_path, parent_id, symbols, raw_edges),
            "assignment" => {
                extract_constant_assignment(
                    source, &child, file_path, parent_id, Some(container_name), symbols,
                );
            }
            "call" | "identifier" => {
                // Two cases handled here:
                //   1. bareword toggle: `private` / `protected` / `public` (no args)
                //   2. targeted form:  `private :foo, :bar`
                match classify_visibility_directive(source, &child) {
                    VisibilityDirective::Toggle(v) => current_visibility = v,
                    VisibilityDirective::Targeted(v, names) => {
                        for n in names {
                            targeted_overrides.insert(n, v);
                        }
                    }
                    VisibilityDirective::None => {
                        if child.kind() == "call" {
                            // Try Rails-aware DSL recognition first (has_many,
                            // before_action, scope, attr_accessor, include, …).
                            // Falls through to require-call recognition when
                            // the call doesn't match any Rails pattern.
                            let consumed = super::ruby_rails::recognize_rails_pattern(
                                source, &child, file_path, parent_id,
                                container_name, current_visibility,
                                symbols, raw_edges,
                            );
                            if !consumed {
                                extract_require_call(source, &child, file_path, parent_id, symbols, raw_edges);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Apply targeted-form overrides last so they win over the toggle state.
    for (name, vis) in targeted_overrides {
        if let Some(indices) = method_indices.get(&name) {
            for &i in indices {
                symbols[i].visibility = vis;
            }
        }
    }
}

/// Classify a body-level call/identifier as a visibility directive.
fn classify_visibility_directive(
    source: &[u8],
    node: &tree_sitter::Node,
) -> VisibilityDirective {
    // Bareword form: a standalone identifier "private" / "protected" / "public".
    if node.kind() == "identifier" {
        return match node_text(source, node) {
            "private" => VisibilityDirective::Toggle(Visibility::Private),
            "protected" => VisibilityDirective::Toggle(Visibility::Protected),
            "public" => VisibilityDirective::Toggle(Visibility::Public),
            _ => VisibilityDirective::None,
        };
    }

    // Call form: bareword call with method=private/protected/public.
    if node.kind() != "call" {
        return VisibilityDirective::None;
    }
    // Skip if there's an explicit receiver — `obj.private` is not a directive.
    if child_by_field(node, "receiver").is_some() {
        return VisibilityDirective::None;
    }
    let method_node = match child_by_field(node, "method") {
        Some(n) => n,
        None => return VisibilityDirective::None,
    };
    let vis = match node_text(source, &method_node) {
        "private" => Visibility::Private,
        "protected" => Visibility::Protected,
        "public" => Visibility::Public,
        _ => return VisibilityDirective::None,
    };

    // No arguments → toggle. With args (symbols) → targeted form.
    let args = match child_by_field(node, "arguments") {
        Some(a) => a,
        None => return VisibilityDirective::Toggle(vis),
    };

    let mut names = Vec::new();
    let mut c = args.walk();
    for arg in args.children(&mut c) {
        if !arg.is_named() {
            continue;
        }
        // `:foo` is a `simple_symbol` (e.g., text ":foo") — strip the leading colon.
        let txt = node_text(source, &arg);
        if let Some(stripped) = txt.strip_prefix(':') {
            names.push(stripped.to_string());
        } else if arg.kind() == "string" {
            // `private "foo"` — also valid, though rare.
            names.push(txt.trim_matches(|c| c == '"' || c == '\'').to_string());
        }
    }

    if names.is_empty() {
        // `private(...)` with non-symbol args — treat as toggle to be safe.
        VisibilityDirective::Toggle(vis)
    } else {
        VisibilityDirective::Targeted(vis, names)
    }
}

enum VisibilityDirective {
    None,
    Toggle(Visibility),
    Targeted(Visibility, Vec<String>),
}

/// Extract a `def name(...) ... end` instance method (or top-level function).
/// Returns the bare method name on success so the caller can apply later
/// targeted-visibility overrides.
fn extract_instance_method(
    source: &[u8],
    node: &tree_sitter::Node,
    file_path: &Path,
    parent_id: NodeId,
    container_name: Option<&str>,
    visibility: Visibility,
    symbols: &mut Vec<SymbolNode>,
    raw_edges: &mut Vec<RawEdge>,
) -> Option<String> {
    let name_node = child_by_field(node, "name")?;
    let name = node_text(source, &name_node).to_string();

    let params = child_by_field(node, "parameters")
        .map(|p| node_text(source, &p).to_string())
        .unwrap_or_else(|| "()".to_string());

    let (kind, qualified) = match container_name {
        Some(cn) => {
            let qkind = if name == "initialize" {
                SymbolKind::Constructor
            } else {
                SymbolKind::Method
            };
            (qkind, format!("{cn}.{name}"))
        }
        None => (SymbolKind::Function, name.clone()),
    };

    let id = NodeId::new(
        &file_path.to_string_lossy(),
        &qualified,
        kind,
        node.start_position().row as u32,
    );

    symbols.push(SymbolNode {
        id,
        name: qualified.clone(),
        kind,
        file_path: file_path.to_path_buf(),
        line_range: (node.start_position().row as u32, node.end_position().row as u32),
        signature: Some(format!("def {name}{params}")),
        doc_comment: extract_doc_comment_anchor(source, node),
        visibility,
        language: Language::Ruby,
        parent: Some(parent_id),
    });

    if let Some(body) = child_by_field(node, "body") {
        extract_calls(source, &body, id, container_name, raw_edges);
    }

    Some(name)
}

/// Extract `def self.name(...) ... end` — a class-level method on the
/// enclosing module/class.  Stored as `Container.name` (matching the
/// project's existing convention used by Go/Python/etc.).
fn extract_singleton_method(
    source: &[u8],
    node: &tree_sitter::Node,
    file_path: &Path,
    parent_id: NodeId,
    container_name: Option<&str>,
    symbols: &mut Vec<SymbolNode>,
    raw_edges: &mut Vec<RawEdge>,
) {
    let name_node = match child_by_field(node, "name") {
        Some(n) => n,
        None => return,
    };
    let name = node_text(source, &name_node).to_string();

    // The receiver of the singleton method.  Most commonly `self`, but can
    // also be an explicit class name like `def Foo.bar`.
    let object = child_by_field(node, "object")
        .map(|o| node_text(source, &o).to_string())
        .unwrap_or_else(|| "self".to_string());

    let params = child_by_field(node, "parameters")
        .map(|p| node_text(source, &p).to_string())
        .unwrap_or_else(|| "()".to_string());

    let qualified = match (container_name, object.as_str()) {
        (Some(cn), "self") => format!("{cn}.{name}"),
        // Explicit receiver like `def OtherClass.foo` — qualify with that receiver.
        (_, recv) if recv != "self" => format!("{recv}.{name}"),
        (None, "self") => name.clone(),
        _ => name.clone(),
    };

    let id = NodeId::new(
        &file_path.to_string_lossy(),
        &qualified,
        SymbolKind::Method,
        node.start_position().row as u32,
    );

    symbols.push(SymbolNode {
        id,
        name: qualified,
        kind: SymbolKind::Method,
        file_path: file_path.to_path_buf(),
        line_range: (node.start_position().row as u32, node.end_position().row as u32),
        signature: Some(format!("def {object}.{name}{params}")),
        doc_comment: extract_doc_comment_anchor(source, node),
        // Singleton methods are always public — the `private`/`protected`
        // toggles in the class body don't apply to them.
        visibility: Visibility::Public,
        language: Language::Ruby,
        parent: Some(parent_id),
    });

    if let Some(body) = child_by_field(node, "body") {
        extract_calls(source, &body, id, container_name, raw_edges);
    }
}

/// `FOO = ...` at file/module/class scope — emit a Constant symbol.
/// Uppercase-leading lhs is what makes it a constant in Ruby.
fn extract_constant_assignment(
    source: &[u8],
    node: &tree_sitter::Node,
    file_path: &Path,
    parent_id: NodeId,
    container_name: Option<&str>,
    symbols: &mut Vec<SymbolNode>,
) {
    let lhs = match child_by_field(node, "left") {
        Some(n) => n,
        None => return,
    };
    if lhs.kind() != "constant" {
        return;
    }
    let name = node_text(source, &lhs).to_string();

    let qualified = match container_name {
        Some(cn) => format!("{cn}::{name}"),
        None => name.clone(),
    };

    let value_text = child_by_field(node, "right")
        .map(|v| format!(" = {}", node_text(source, &v)))
        .unwrap_or_default();

    let id = NodeId::new(
        &file_path.to_string_lossy(),
        &qualified,
        SymbolKind::Constant,
        node.start_position().row as u32,
    );

    symbols.push(SymbolNode {
        id,
        name: qualified,
        kind: SymbolKind::Constant,
        file_path: file_path.to_path_buf(),
        line_range: (node.start_position().row as u32, node.end_position().row as u32),
        signature: Some(format!("{name}{value_text}")),
        doc_comment: extract_doc_comment_anchor(source, node),
        visibility: Visibility::Public,
        language: Language::Ruby,
        parent: Some(parent_id),
    });
}

/// `require "x"` / `require_relative "x"` / `load "x"` → IMPORTS edge.
fn extract_require_call(
    source: &[u8],
    node: &tree_sitter::Node,
    file_path: &Path,
    parent_id: NodeId,
    symbols: &mut Vec<SymbolNode>,
    raw_edges: &mut Vec<RawEdge>,
) {
    if child_by_field(node, "receiver").is_some() {
        return;
    }
    let method_node = match child_by_field(node, "method") {
        Some(n) => n,
        None => return,
    };
    let method_name = node_text(source, &method_node);
    if !matches!(method_name, "require" | "require_relative" | "load" | "autoload") {
        return;
    }

    let args = match child_by_field(node, "arguments") {
        Some(a) => a,
        None => return,
    };

    // Take the last string-shaped argument as the import path (autoload's
    // first arg is a symbol, second is the path; `require` uses just the path).
    let mut path: Option<String> = None;
    let mut c = args.walk();
    for arg in args.children(&mut c) {
        if arg.kind() == "string" {
            let raw = node_text(source, &arg);
            path = Some(raw.trim_matches(|ch| ch == '"' || ch == '\'').to_string());
        }
    }
    let path = match path {
        Some(p) if !p.is_empty() => p,
        _ => return,
    };

    let id = NodeId::new(
        &file_path.to_string_lossy(),
        &path,
        SymbolKind::Import,
        node.start_position().row as u32,
    );

    symbols.push(SymbolNode {
        id,
        name: path.clone(),
        kind: SymbolKind::Import,
        file_path: file_path.to_path_buf(),
        line_range: (node.start_position().row as u32, node.end_position().row as u32),
        signature: Some(format!("{method_name} \"{path}\"")),
        doc_comment: None,
        visibility: Visibility::Public,
        language: Language::Ruby,
        parent: Some(parent_id),
    });

    raw_edges.push(RawEdge {
        source: parent_id,
        kind: EdgeKind::Imports,
        target_name: path,
        target_module: None,
        source_line: node.start_position().row as u32,
    });
}

/// Walk a function/method body collecting CALLS edges.  `self.x()` is
/// qualified to `Container.x` (mirrors Go/Python behavior); chained calls
/// like `a.b.c` are left as raw text for the resolver to fall back on.
fn extract_calls(
    source: &[u8],
    node: &tree_sitter::Node,
    caller_id: NodeId,
    container_name: Option<&str>,
    raw_edges: &mut Vec<RawEdge>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "call" {
            if let Some(target) = build_call_target(source, &child, container_name) {
                raw_edges.push(RawEdge {
                    source: caller_id,
                    kind: EdgeKind::Calls,
                    target_name: target,
                    target_module: None,
                    source_line: child.start_position().row as u32,
                });
            }
        }
        extract_calls(source, &child, caller_id, container_name, raw_edges);
    }
}

/// Reconstruct a call target string from a `call` node.  Matches the shape
/// the resolver expects (`name`, `obj.name`, `Class.name`).
fn build_call_target(
    source: &[u8],
    call: &tree_sitter::Node,
    container_name: Option<&str>,
) -> Option<String> {
    let method = child_by_field(call, "method")?;
    let method_name = node_text(source, &method);

    // Skip directives we already consumed at the body-walk layer so they
    // don't leak in as CALLS edges from method bodies (they only appear in
    // class bodies, but be defensive).
    if matches!(method_name, "private" | "protected" | "public")
        && child_by_field(call, "receiver").is_none()
    {
        return None;
    }

    match child_by_field(call, "receiver") {
        Some(recv) => {
            let recv_text = node_text(source, &recv);
            // `self.foo` → `Container.foo`
            if recv_text == "self" {
                if let Some(cn) = container_name {
                    return Some(format!("{cn}.{method_name}"));
                }
            }
            // Anything else (`obj.foo`, `Class.foo`, chained selectors) — keep
            // raw text; the resolver does name-only fallback.
            Some(format!("{recv_text}.{method_name}"))
        }
        None => Some(method_name.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extractor::LanguageExtractor;

    fn extract(src: &str) -> (Vec<SymbolNode>, Vec<RawEdge>) {
        let extractor = RubyExtractor;
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&extractor.tree_sitter_language())
            .expect("tree-sitter-ruby load failed");
        let tree = parser.parse(src.as_bytes(), None).expect("parse failed");
        let r = extractor.extract(src.as_bytes(), &tree, Path::new("test.rb"));
        (r.symbols, r.raw_edges)
    }

    fn find<'a>(syms: &'a [SymbolNode], name: &str) -> Option<&'a SymbolNode> {
        syms.iter().find(|s| s.name == name)
    }

    fn calls_from<'a>(edges: &'a [RawEdge], src: NodeId) -> Vec<&'a str> {
        edges.iter()
            .filter(|e| e.source == src && e.kind == EdgeKind::Calls)
            .map(|e| e.target_name.as_str())
            .collect()
    }

    // ── module / class ───────────────────────────────────────────────────────

    #[test]
    fn extracts_module() {
        let (syms, _) = extract("module Foo\nend\n");
        let m = find(&syms, "Foo").expect("Foo missing");
        assert_eq!(m.kind, SymbolKind::Module);
        assert_eq!(m.language, Language::Ruby);
        assert_eq!(m.signature.as_deref(), Some("module Foo"));
    }

    #[test]
    fn extracts_class() {
        let (syms, _) = extract("class Bar\nend\n");
        let c = find(&syms, "Bar").expect("Bar missing");
        assert_eq!(c.kind, SymbolKind::Class);
        assert_eq!(c.signature.as_deref(), Some("class Bar"));
    }

    #[test]
    fn extracts_class_with_superclass_and_extends_edge() {
        let src = "class Dog < Animal\nend\n";
        let (syms, edges) = extract(src);
        let c = find(&syms, "Dog").expect("Dog missing");
        assert_eq!(c.signature.as_deref(), Some("class Dog < Animal"));

        let ext: Vec<_> = edges.iter()
            .filter(|e| e.kind == EdgeKind::Extends && e.target_name == "Animal")
            .collect();
        assert_eq!(ext.len(), 1, "expected one EXTENDS Animal edge");
    }

    // ── methods ──────────────────────────────────────────────────────────────

    #[test]
    fn extracts_top_level_def_as_function() {
        let (syms, _) = extract("def hello\nend\n");
        let f = find(&syms, "hello").expect("hello missing");
        assert_eq!(f.kind, SymbolKind::Function);
        assert_eq!(f.visibility, Visibility::Public);
    }

    #[test]
    fn extracts_instance_method_qualified_with_class() {
        let src = "class Dog\n  def bark\n  end\nend\n";
        let (syms, _) = extract(src);
        let m = find(&syms, "Dog.bark").expect("Dog.bark missing");
        assert_eq!(m.kind, SymbolKind::Method);
        assert_eq!(m.visibility, Visibility::Public);
    }

    #[test]
    fn initialize_is_constructor() {
        let src = "class Dog\n  def initialize(name)\n  end\nend\n";
        let (syms, _) = extract(src);
        let m = find(&syms, "Dog.initialize").expect("Dog.initialize missing");
        assert_eq!(m.kind, SymbolKind::Constructor);
    }

    #[test]
    fn extracts_singleton_method_as_class_method() {
        let src = "class Dog\n  def self.create(name)\n  end\nend\n";
        let (syms, _) = extract(src);
        let m = find(&syms, "Dog.create").expect("Dog.create missing");
        assert_eq!(m.kind, SymbolKind::Method);
        // `def self.x` is always public regardless of `private` toggle.
        assert_eq!(m.visibility, Visibility::Public);
    }

    // ── visibility (option B: full fidelity) ────────────────────────────────

    #[test]
    fn private_keyword_toggles_subsequent_methods() {
        let src = "class Foo\n  def public_a\n  end\n  private\n  def private_a\n  end\n  def private_b\n  end\nend\n";
        let (syms, _) = extract(src);
        assert_eq!(find(&syms, "Foo.public_a").unwrap().visibility, Visibility::Public);
        assert_eq!(find(&syms, "Foo.private_a").unwrap().visibility, Visibility::Private);
        assert_eq!(find(&syms, "Foo.private_b").unwrap().visibility, Visibility::Private);
    }

    #[test]
    fn public_keyword_resets_after_private() {
        let src = "class Foo\n  private\n  def hidden\n  end\n  public\n  def shown\n  end\nend\n";
        let (syms, _) = extract(src);
        assert_eq!(find(&syms, "Foo.hidden").unwrap().visibility, Visibility::Private);
        assert_eq!(find(&syms, "Foo.shown").unwrap().visibility, Visibility::Public);
    }

    #[test]
    fn protected_keyword_toggles_subsequent_methods() {
        let src = "class Foo\n  protected\n  def helper\n  end\nend\n";
        let (syms, _) = extract(src);
        assert_eq!(find(&syms, "Foo.helper").unwrap().visibility, Visibility::Protected);
    }

    #[test]
    fn private_with_symbol_args_marks_only_named_methods() {
        // `private :secret` marks ONLY :secret as private; other methods stay public.
        let src = "class Foo\n  def public_one\n  end\n  def secret\n  end\n  def public_two\n  end\n  private :secret\nend\n";
        let (syms, _) = extract(src);
        assert_eq!(find(&syms, "Foo.public_one").unwrap().visibility, Visibility::Public);
        assert_eq!(find(&syms, "Foo.secret").unwrap().visibility, Visibility::Private);
        assert_eq!(find(&syms, "Foo.public_two").unwrap().visibility, Visibility::Public);
    }

    #[test]
    fn singleton_method_unaffected_by_private_toggle() {
        let src = "class Foo\n  private\n  def self.factory\n  end\n  def hidden\n  end\nend\n";
        let (syms, _) = extract(src);
        // singleton stays public; instance method after toggle is private
        assert_eq!(find(&syms, "Foo.factory").unwrap().visibility, Visibility::Public);
        assert_eq!(find(&syms, "Foo.hidden").unwrap().visibility, Visibility::Private);
    }

    // ── constants ────────────────────────────────────────────────────────────

    #[test]
    fn extracts_top_level_constant() {
        let (syms, _) = extract("MAX = 100\n");
        let c = find(&syms, "MAX").expect("MAX missing");
        assert_eq!(c.kind, SymbolKind::Constant);
    }

    #[test]
    fn extracts_class_scoped_constant() {
        let src = "class Foo\n  MAX = 5\nend\n";
        let (syms, _) = extract(src);
        let c = find(&syms, "Foo::MAX").expect("Foo::MAX missing");
        assert_eq!(c.kind, SymbolKind::Constant);
    }

    #[test]
    fn lowercase_assignment_is_not_a_constant() {
        // Assignments to lowercase identifiers are local variables, not constants.
        let (syms, _) = extract("x = 1\n");
        assert!(syms.iter().all(|s| s.kind != SymbolKind::Constant));
    }

    // ── imports ──────────────────────────────────────────────────────────────

    #[test]
    fn extracts_require_as_import_with_edge() {
        let src = "require \"json\"\n";
        let (syms, edges) = extract(src);
        let i = find(&syms, "json").expect("import json missing");
        assert_eq!(i.kind, SymbolKind::Import);

        let imports: Vec<_> = edges.iter()
            .filter(|e| e.kind == EdgeKind::Imports && e.target_name == "json")
            .collect();
        assert_eq!(imports.len(), 1);
    }

    #[test]
    fn extracts_require_relative() {
        let src = "require_relative \"./helpers\"\n";
        let (_, edges) = extract(src);
        assert!(edges.iter().any(|e| e.kind == EdgeKind::Imports && e.target_name == "./helpers"));
    }

    // ── calls ────────────────────────────────────────────────────────────────

    #[test]
    fn emits_calls_edge_for_bare_call() {
        let src = "def helper\nend\ndef run\n  helper()\nend\n";
        let (syms, edges) = extract(src);
        let run = find(&syms, "run").expect("run missing");
        let targets = calls_from(&edges, run.id);
        assert!(targets.contains(&"helper"), "expected CALLS helper, got {targets:?}");
    }

    #[test]
    fn qualifies_self_call_with_class() {
        let src = "class Foo\n  def bar\n    self.baz\n  end\n  def baz\n  end\nend\n";
        let (syms, edges) = extract(src);
        let bar = find(&syms, "Foo.bar").expect("Foo.bar missing");
        let targets = calls_from(&edges, bar.id);
        assert!(
            targets.contains(&"Foo.baz"),
            "expected CALLS Foo.baz, got {targets:?}"
        );
    }

    #[test]
    fn preserves_external_receiver_calls() {
        let src = "class Foo\n  def bar\n    Logger.info(\"hi\")\n  end\nend\n";
        let (syms, edges) = extract(src);
        let bar = find(&syms, "Foo.bar").expect("Foo.bar missing");
        let targets = calls_from(&edges, bar.id);
        assert!(
            targets.contains(&"Logger.info"),
            "expected CALLS Logger.info, got {targets:?}"
        );
    }

    // ── doc comments ─────────────────────────────────────────────────────────

    #[test]
    fn extracts_preceding_doc_comment_on_class() {
        let src = "# Represents a dog.\nclass Dog\nend\n";
        let (syms, _) = extract(src);
        let c = find(&syms, "Dog").expect("Dog missing");
        assert_eq!(c.doc_comment.as_deref(), Some("Represents a dog."));
    }

    #[test]
    fn extracts_multiline_doc_comment_on_method() {
        let src = "class Dog\n  # Bark loudly.\n  # Returns a string.\n  def bark\n  end\nend\n";
        let (syms, _) = extract(src);
        let m = find(&syms, "Dog.bark").expect("Dog.bark missing");
        let doc = m.doc_comment.as_deref().unwrap_or("");
        assert!(doc.contains("Bark loudly."), "doc was: {doc:?}");
        assert!(doc.contains("Returns a string."), "doc was: {doc:?}");
    }

    // ── full-file smoke test ─────────────────────────────────────────────────

    #[test]
    fn full_file_smoke_test() {
        let src = r#"# A small order module.
require "json"
require_relative "./logger"

MAX_ITEMS = 100

module Orders
  class Order
    PENDING = "pending"

    def initialize(id)
      @id = id
    end

    def submit
      self.validate
      Logger.info("submitted")
    end

    def self.create(id)
      Order.new(id)
    end

    private

    def validate
      raise "bad" unless @id
    end
  end
end
"#;
        let (syms, edges) = extract(src);

        assert!(find(&syms, "Orders").is_some(), "module Orders missing");
        assert!(find(&syms, "Order").is_some(), "class Order missing");
        assert_eq!(find(&syms, "Order.initialize").unwrap().kind, SymbolKind::Constructor);
        assert_eq!(find(&syms, "Order.submit").unwrap().visibility, Visibility::Public);
        assert_eq!(find(&syms, "Order.create").unwrap().visibility, Visibility::Public);
        assert_eq!(find(&syms, "Order.validate").unwrap().visibility, Visibility::Private);
        assert_eq!(find(&syms, "MAX_ITEMS").unwrap().kind, SymbolKind::Constant);
        assert_eq!(find(&syms, "Order::PENDING").unwrap().kind, SymbolKind::Constant);

        // imports
        let import_targets: Vec<_> = edges.iter()
            .filter(|e| e.kind == EdgeKind::Imports)
            .map(|e| e.target_name.as_str()).collect();
        assert!(import_targets.contains(&"json"));
        assert!(import_targets.contains(&"./logger"));

        // self.validate inside submit → Order.validate
        let submit = find(&syms, "Order.submit").unwrap();
        let submit_calls = calls_from(&edges, submit.id);
        assert!(submit_calls.contains(&"Order.validate"),
            "expected Order.validate in submit calls, got {submit_calls:?}");
        assert!(submit_calls.contains(&"Logger.info"),
            "expected Logger.info in submit calls, got {submit_calls:?}");
    }
}
