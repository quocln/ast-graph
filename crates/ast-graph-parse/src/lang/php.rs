use ast_graph_core::*;
use crate::extractor::*;
use std::path::Path;

pub struct PhpExtractor;

impl LanguageExtractor for PhpExtractor {
    fn language(&self) -> Language {
        Language::Php
    }

    fn file_extensions(&self) -> &[&str] {
        &["php", "phtml"]
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_php::LANGUAGE_PHP.into()
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
            language: Language::Php,
            parent: None,
        });

        walk_top(source, &tree.root_node(), file_path, file_node_id, None, &mut symbols, &mut raw_edges);
        ExtractResult { symbols, raw_edges }
    }
}

/// Walk top-level program children. `current_ns` carries the in-effect
/// namespace from a previous body-less `namespace Foo;` declaration so all
/// subsequent siblings get qualified with it.
fn walk_top(
    source: &[u8],
    node: &tree_sitter::Node,
    file_path: &Path,
    parent_id: NodeId,
    initial_ns: Option<&str>,
    symbols: &mut Vec<SymbolNode>,
    raw_edges: &mut Vec<RawEdge>,
) {
    let mut current_ns: Option<String> = initial_ns.map(|s| s.to_string());

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "namespace_definition" => {
                let ns_name = child_by_field(&child, "name")
                    .map(|n| node_text(source, &n).to_string());
                emit_namespace_symbol(source, &child, file_path, parent_id, ns_name.as_deref(), symbols);
                if let Some(body) = child_by_field(&child, "body") {
                    // `namespace Foo { ... }` — body-scoped.
                    walk_top(source, &body, file_path, parent_id, ns_name.as_deref(), symbols, raw_edges);
                } else {
                    // `namespace Foo;` — applies to rest of file.
                    current_ns = ns_name;
                }
            }
            "namespace_use_declaration" => {
                extract_use_declaration(source, &child, file_path, parent_id, symbols, raw_edges);
            }
            "class_declaration" => {
                extract_class_like(
                    source, &child, file_path, parent_id, current_ns.as_deref(),
                    SymbolKind::Class, "class", symbols, raw_edges,
                );
            }
            "interface_declaration" => {
                extract_class_like(
                    source, &child, file_path, parent_id, current_ns.as_deref(),
                    SymbolKind::Interface, "interface", symbols, raw_edges,
                );
            }
            "trait_declaration" => {
                extract_class_like(
                    source, &child, file_path, parent_id, current_ns.as_deref(),
                    SymbolKind::Trait, "trait", symbols, raw_edges,
                );
            }
            "enum_declaration" => {
                extract_enum(source, &child, file_path, parent_id, current_ns.as_deref(), symbols, raw_edges);
            }
            "function_definition" => {
                extract_function(source, &child, file_path, parent_id, current_ns.as_deref(), symbols, raw_edges);
            }
            "const_declaration" => {
                extract_top_level_const(source, &child, file_path, parent_id, current_ns.as_deref(), symbols);
            }
            // Recurse through compound bodies inside `namespace Foo { ... }`
            "compound_statement" => {
                walk_top(source, &child, file_path, parent_id, current_ns.as_deref(), symbols, raw_edges);
            }
            // Scan any other statement for inline arrow functions / anonymous classes
            // (`$f = fn($x) => ...;`, `$x = new class { ... };`).  extract_calls
            // handles both node types recursively; container_qualified=None at file scope.
            _ => {
                extract_calls(source, &child, parent_id, None, file_path, symbols, raw_edges);
            }
        }
    }
}

fn emit_namespace_symbol(
    source: &[u8],
    node: &tree_sitter::Node,
    file_path: &Path,
    parent_id: NodeId,
    name: Option<&str>,
    symbols: &mut Vec<SymbolNode>,
) {
    let name = match name {
        Some(n) => n.to_string(),
        None => return,
    };
    let id = NodeId::new(
        &file_path.to_string_lossy(),
        &name,
        SymbolKind::Namespace,
        node.start_position().row as u32,
    );
    symbols.push(SymbolNode {
        id,
        name: name.clone(),
        kind: SymbolKind::Namespace,
        file_path: file_path.to_path_buf(),
        line_range: (node.start_position().row as u32, node.end_position().row as u32),
        signature: Some(format!("namespace {name}")),
        doc_comment: extract_preceding_doc_comment(source, node),
        visibility: Visibility::Public,
        language: Language::Php,
        parent: Some(parent_id),
    });
}

/// `use Foo\Bar;` / `use Foo\Bar as B;` / `use Foo\{A, B as C};`
fn extract_use_declaration(
    source: &[u8],
    node: &tree_sitter::Node,
    file_path: &Path,
    parent_id: NodeId,
    symbols: &mut Vec<SymbolNode>,
    raw_edges: &mut Vec<RawEdge>,
) {
    // The grammar shape is roughly:
    //   namespace_use_declaration
    //     ├── ('function' | 'const')?    — optional `type` token
    //     ├── namespace_use_clause +     — for non-grouped form
    //     └── (namespace_name ... namespace_use_group)?  — for grouped form
    //
    // Handle both shapes by walking children.
    let mut group_prefix: Option<String> = None;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "namespace_name" => {
                // Prefix for grouped form: `use Foo\Bar\{A, B}`
                group_prefix = Some(node_text(source, &child).to_string());
            }
            "namespace_use_clause" => {
                // Walk the clause: namespace_name + optional namespace_aliasing_clause
                let mut path: Option<String> = None;
                let mut alias: Option<String> = None;
                let mut c2 = child.walk();
                for sub in child.children(&mut c2) {
                    match sub.kind() {
                        "qualified_name" | "namespace_name" | "name" => {
                            path = Some(node_text(source, &sub).to_string());
                        }
                        "namespace_aliasing_clause" => {
                            // `as Alias` — the alias identifier is the last named child.
                            let mut c3 = sub.walk();
                            if let Some(id) = sub.children(&mut c3).filter(|n| n.is_named()).last() {
                                alias = Some(node_text(source, &id).to_string());
                            }
                        }
                        _ => {}
                    }
                }
                if let Some(p) = path {
                    emit_use_import(source, &child, file_path, parent_id, &p, alias.as_deref(), symbols, raw_edges);
                }
            }
            "namespace_use_group" => {
                let mut c2 = child.walk();
                for sub in child.children(&mut c2) {
                    if sub.kind() != "namespace_use_clause" && sub.kind() != "namespace_use_group_clause" {
                        continue;
                    }
                    let mut tail: Option<String> = None;
                    let mut alias: Option<String> = None;
                    let mut c3 = sub.walk();
                    for ssub in sub.children(&mut c3) {
                        match ssub.kind() {
                            "qualified_name" | "namespace_name" | "name" => {
                                tail = Some(node_text(source, &ssub).to_string());
                            }
                            "namespace_aliasing_clause" => {
                                let mut c4 = ssub.walk();
                                if let Some(id) = ssub.children(&mut c4).filter(|n| n.is_named()).last() {
                                    alias = Some(node_text(source, &id).to_string());
                                }
                            }
                            _ => {}
                        }
                    }
                    if let Some(t) = tail {
                        let full = match &group_prefix {
                            Some(p) => format!("{p}\\{t}"),
                            None => t,
                        };
                        emit_use_import(source, &sub, file_path, parent_id, &full, alias.as_deref(), symbols, raw_edges);
                    }
                }
            }
            _ => {}
        }
    }
}

fn emit_use_import(
    source: &[u8],
    node: &tree_sitter::Node,
    file_path: &Path,
    parent_id: NodeId,
    path: &str,
    alias: Option<&str>,
    symbols: &mut Vec<SymbolNode>,
    raw_edges: &mut Vec<RawEdge>,
) {
    let _ = source;
    let id = NodeId::new(
        &file_path.to_string_lossy(),
        path,
        SymbolKind::Import,
        node.start_position().row as u32,
    );
    let signature = match alias {
        Some(a) => format!("use {path} as {a}"),
        None => format!("use {path}"),
    };
    symbols.push(SymbolNode {
        id,
        name: path.to_string(),
        kind: SymbolKind::Import,
        file_path: file_path.to_path_buf(),
        line_range: (node.start_position().row as u32, node.end_position().row as u32),
        signature: Some(signature),
        doc_comment: None,
        visibility: Visibility::Public,
        language: Language::Php,
        parent: Some(parent_id),
    });
    raw_edges.push(RawEdge {
        source: parent_id,
        kind: EdgeKind::Imports,
        target_name: path.to_string(),
        target_module: None,
        source_line: node.start_position().row as u32,
    });
}

/// Extract class/interface/trait — they share the same shape.
fn extract_class_like(
    source: &[u8],
    node: &tree_sitter::Node,
    file_path: &Path,
    parent_id: NodeId,
    namespace: Option<&str>,
    kind: SymbolKind,
    keyword: &str,
    symbols: &mut Vec<SymbolNode>,
    raw_edges: &mut Vec<RawEdge>,
) {
    let name_node = match child_by_field(node, "name") {
        Some(n) => n,
        None => return,
    };
    let bare_name = node_text(source, &name_node).to_string();
    let qualified = qualify_with_ns(&bare_name, namespace);

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
        signature: Some(format!("{keyword} {bare_name}")),
        doc_comment: extract_preceding_doc_comment(source, node),
        visibility: Visibility::Public,
        language: Language::Php,
        parent: Some(parent_id),
    });

    // `extends` — `base_clause`. PHP class can only extend one parent;
    // `class_interface_clause` is for `implements`.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "base_clause" => {
                emit_inheritance_edges(source, &child, id, EdgeKind::Extends, raw_edges);
            }
            "class_interface_clause" => {
                emit_inheritance_edges(source, &child, id, EdgeKind::Implements, raw_edges);
            }
            _ => {}
        }
    }

    if let Some(body) = child_by_field(node, "body") {
        walk_class_body(source, &body, file_path, id, &qualified, symbols, raw_edges);
    }
}

fn emit_inheritance_edges(
    source: &[u8],
    clause: &tree_sitter::Node,
    source_id: NodeId,
    kind: EdgeKind,
    raw_edges: &mut Vec<RawEdge>,
) {
    let mut cursor = clause.walk();
    for child in clause.children(&mut cursor) {
        if matches!(child.kind(), "qualified_name" | "namespace_name" | "name") {
            raw_edges.push(RawEdge {
                source: source_id,
                kind,
                target_name: node_text(source, &child).to_string(),
                target_module: None,
                source_line: clause.start_position().row as u32,
            });
        }
    }
}

/// Walk a declaration_list: collects methods, properties, constants, and
/// `use Trait1, Trait2;` statements (which become IMPLEMENTS edges).
fn walk_class_body(
    source: &[u8],
    body: &tree_sitter::Node,
    file_path: &Path,
    parent_id: NodeId,
    container_qualified: &str,
    symbols: &mut Vec<SymbolNode>,
    raw_edges: &mut Vec<RawEdge>,
) {
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        match child.kind() {
            "method_declaration" => {
                extract_method(source, &child, file_path, parent_id, container_qualified, symbols, raw_edges);
            }
            "property_declaration" => {
                extract_property(source, &child, file_path, parent_id, container_qualified, symbols);
            }
            "const_declaration" => {
                extract_class_const(source, &child, file_path, parent_id, container_qualified, symbols);
            }
            "use_declaration" => {
                // `use Trait1, Trait2;` inside a class body — emit IMPLEMENTS
                // edges (closest semantic match for trait composition).
                let mut c2 = child.walk();
                for sub in child.children(&mut c2) {
                    if matches!(sub.kind(), "qualified_name" | "namespace_name" | "name") {
                        raw_edges.push(RawEdge {
                            source: parent_id,
                            kind: EdgeKind::Implements,
                            target_name: node_text(source, &sub).to_string(),
                            target_module: None,
                            source_line: child.start_position().row as u32,
                        });
                    }
                }
            }
            _ => {}
        }
    }
}

/// Find `visibility_modifier` text among a node's children.
fn read_visibility(source: &[u8], node: &tree_sitter::Node) -> Visibility {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            return match node_text(source, &child) {
                "public" => Visibility::Public,
                "protected" => Visibility::Protected,
                "private" => Visibility::Private,
                _ => Visibility::Public,
            };
        }
    }
    Visibility::Public
}

fn extract_method(
    source: &[u8],
    node: &tree_sitter::Node,
    file_path: &Path,
    parent_id: NodeId,
    container_qualified: &str,
    symbols: &mut Vec<SymbolNode>,
    raw_edges: &mut Vec<RawEdge>,
) {
    let name_node = match child_by_field(node, "name") {
        Some(n) => n,
        None => return,
    };
    let name = node_text(source, &name_node).to_string();

    let params = child_by_field(node, "parameters")
        .map(|p| node_text(source, &p).to_string())
        .unwrap_or_else(|| "()".to_string());

    let return_type = child_by_field(node, "return_type")
        .map(|r| format!(": {}", node_text(source, &r)))
        .unwrap_or_default();

    let kind = if name == "__construct" {
        SymbolKind::Constructor
    } else {
        SymbolKind::Method
    };
    let qualified = format!("{container_qualified}.{name}");

    let visibility = read_visibility(source, node);

    let id = NodeId::new(
        &file_path.to_string_lossy(),
        &qualified,
        kind,
        node.start_position().row as u32,
    );

    symbols.push(SymbolNode {
        id,
        name: qualified,
        kind,
        file_path: file_path.to_path_buf(),
        line_range: (node.start_position().row as u32, node.end_position().row as u32),
        signature: Some(format!("function {name}{params}{return_type}")),
        doc_comment: extract_preceding_doc_comment(source, node),
        visibility,
        language: Language::Php,
        parent: Some(parent_id),
    });

    if let Some(body) = child_by_field(node, "body") {
        extract_calls(source, &body, id, Some(container_qualified), file_path, symbols, raw_edges);
    }
}

fn extract_function(
    source: &[u8],
    node: &tree_sitter::Node,
    file_path: &Path,
    parent_id: NodeId,
    namespace: Option<&str>,
    symbols: &mut Vec<SymbolNode>,
    raw_edges: &mut Vec<RawEdge>,
) {
    let name_node = match child_by_field(node, "name") {
        Some(n) => n,
        None => return,
    };
    let bare_name = node_text(source, &name_node).to_string();
    let qualified = qualify_with_ns(&bare_name, namespace);

    let params = child_by_field(node, "parameters")
        .map(|p| node_text(source, &p).to_string())
        .unwrap_or_else(|| "()".to_string());

    let return_type = child_by_field(node, "return_type")
        .map(|r| format!(": {}", node_text(source, &r)))
        .unwrap_or_default();

    let id = NodeId::new(
        &file_path.to_string_lossy(),
        &qualified,
        SymbolKind::Function,
        node.start_position().row as u32,
    );

    symbols.push(SymbolNode {
        id,
        name: qualified.clone(),
        kind: SymbolKind::Function,
        file_path: file_path.to_path_buf(),
        line_range: (node.start_position().row as u32, node.end_position().row as u32),
        signature: Some(format!("function {bare_name}{params}{return_type}")),
        doc_comment: extract_preceding_doc_comment(source, node),
        visibility: Visibility::Public,
        language: Language::Php,
        parent: Some(parent_id),
    });

    if let Some(body) = child_by_field(node, "body") {
        extract_calls(source, &body, id, None, file_path, symbols, raw_edges);
    }
}

fn extract_property(
    source: &[u8],
    node: &tree_sitter::Node,
    file_path: &Path,
    parent_id: NodeId,
    container_qualified: &str,
    symbols: &mut Vec<SymbolNode>,
) {
    let visibility = read_visibility(source, node);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "property_element" {
            continue;
        }
        let name_node = match child_by_field(&child, "name") {
            Some(n) => n,
            None => continue,
        };
        // Property names appear with leading `$` — strip for the symbol name.
        let raw = node_text(source, &name_node);
        let bare = raw.trim_start_matches('$').to_string();
        let qualified = format!("{container_qualified}.{bare}");

        let id = NodeId::new(
            &file_path.to_string_lossy(),
            &qualified,
            SymbolKind::Property,
            child.start_position().row as u32,
        );

        symbols.push(SymbolNode {
            id,
            name: qualified,
            kind: SymbolKind::Property,
            file_path: file_path.to_path_buf(),
            line_range: (child.start_position().row as u32, child.end_position().row as u32),
            signature: Some(format!("${bare}")),
            doc_comment: extract_preceding_doc_comment(source, node),
            visibility,
            language: Language::Php,
            parent: Some(parent_id),
        });
    }
}

fn extract_class_const(
    source: &[u8],
    node: &tree_sitter::Node,
    file_path: &Path,
    parent_id: NodeId,
    container_qualified: &str,
    symbols: &mut Vec<SymbolNode>,
) {
    let visibility = read_visibility(source, node);
    extract_const_elements(source, node, file_path, parent_id, Some(container_qualified), visibility, symbols);
}

fn extract_top_level_const(
    source: &[u8],
    node: &tree_sitter::Node,
    file_path: &Path,
    parent_id: NodeId,
    namespace: Option<&str>,
    symbols: &mut Vec<SymbolNode>,
) {
    extract_const_elements(source, node, file_path, parent_id, namespace, Visibility::Public, symbols);
}

fn extract_const_elements(
    source: &[u8],
    node: &tree_sitter::Node,
    file_path: &Path,
    parent_id: NodeId,
    container: Option<&str>,
    visibility: Visibility,
    symbols: &mut Vec<SymbolNode>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "const_element" {
            continue;
        }
        // const_element: name = value; the name is the first named child.
        let mut c2 = child.walk();
        let name_node = match child.children(&mut c2).find(|n| n.is_named() && matches!(n.kind(), "name" | "identifier")) {
            Some(n) => n,
            None => continue,
        };
        let bare = node_text(source, &name_node).to_string();
        let qualified = match container {
            Some(c) if c.contains('.') => format!("{c}::{bare}"),
            // class-scoped const: use `Class::CONST` form (PHP convention)
            Some(c) => format!("{c}::{bare}"),
            None => bare.clone(),
        };

        let id = NodeId::new(
            &file_path.to_string_lossy(),
            &qualified,
            SymbolKind::Constant,
            child.start_position().row as u32,
        );

        symbols.push(SymbolNode {
            id,
            name: qualified,
            kind: SymbolKind::Constant,
            file_path: file_path.to_path_buf(),
            line_range: (child.start_position().row as u32, child.end_position().row as u32),
            signature: Some(format!("const {bare}")),
            doc_comment: None,
            visibility,
            language: Language::Php,
            parent: Some(parent_id),
        });
    }
}

fn extract_enum(
    source: &[u8],
    node: &tree_sitter::Node,
    file_path: &Path,
    parent_id: NodeId,
    namespace: Option<&str>,
    symbols: &mut Vec<SymbolNode>,
    raw_edges: &mut Vec<RawEdge>,
) {
    let name_node = match child_by_field(node, "name") {
        Some(n) => n,
        None => return,
    };
    let bare = node_text(source, &name_node).to_string();
    let qualified = qualify_with_ns(&bare, namespace);

    let id = NodeId::new(
        &file_path.to_string_lossy(),
        &qualified,
        SymbolKind::Enum,
        node.start_position().row as u32,
    );

    symbols.push(SymbolNode {
        id,
        name: qualified.clone(),
        kind: SymbolKind::Enum,
        file_path: file_path.to_path_buf(),
        line_range: (node.start_position().row as u32, node.end_position().row as u32),
        signature: Some(format!("enum {bare}")),
        doc_comment: extract_preceding_doc_comment(source, node),
        visibility: Visibility::Public,
        language: Language::Php,
        parent: Some(parent_id),
    });

    if let Some(body) = child_by_field(node, "body") {
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            match child.kind() {
                "enum_case" => {
                    if let Some(case_name) = child_by_field(&child, "name") {
                        let cn = node_text(source, &case_name).to_string();
                        let case_qualified = format!("{qualified}.{cn}");
                        let case_id = NodeId::new(
                            &file_path.to_string_lossy(),
                            &case_qualified,
                            SymbolKind::EnumVariant,
                            child.start_position().row as u32,
                        );
                        symbols.push(SymbolNode {
                            id: case_id,
                            name: case_qualified,
                            kind: SymbolKind::EnumVariant,
                            file_path: file_path.to_path_buf(),
                            line_range: (child.start_position().row as u32, child.end_position().row as u32),
                            signature: Some(format!("case {cn}")),
                            doc_comment: None,
                            visibility: Visibility::Public,
                            language: Language::Php,
                            parent: Some(id),
                        });
                    }
                }
                "method_declaration" => {
                    extract_method(source, &child, file_path, id, &qualified, symbols, raw_edges);
                }
                _ => {}
            }
        }
    }
}

fn qualify_with_ns(name: &str, namespace: Option<&str>) -> String {
    match namespace {
        Some(ns) if !ns.is_empty() => format!("{ns}\\{name}"),
        _ => name.to_string(),
    }
}

/// Walk a body tree collecting CALLS/REFERENCES edges and emitting inline symbols.
///
/// Handles the three PHP call shapes (function_call_expression, member_call_expression,
/// scoped_call_expression). `$this`/`self`/`static` calls are qualified to the
/// enclosing container.
///
/// PHP 8.1 first-class callable syntax (`strlen(...)`) is detected when arguments
/// contain only `variadic_placeholder` — emits REFERENCES instead of CALLS.
///
/// PHP 7.4+ arrow functions (`arrow_function` node) and anonymous classes
/// (`object_creation_expression` wrapping `anonymous_class`) emit symbols inline
/// and recurse into their bodies.
fn extract_calls(
    source: &[u8],
    node: &tree_sitter::Node,
    caller_id: NodeId,
    container_qualified: Option<&str>,
    file_path: &Path,
    symbols: &mut Vec<SymbolNode>,
    raw_edges: &mut Vec<RawEdge>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_call_expression" => {
                if let Some(f) = child_by_field(&child, "function") {
                    let target_name = node_text(source, &f).to_string();
                    let edge_kind = if args_are_callable_syntax(source, &child) {
                        EdgeKind::References
                    } else {
                        EdgeKind::Calls
                    };
                    raw_edges.push(RawEdge {
                        source: caller_id,
                        kind: edge_kind,
                        target_name,
                        target_module: None,
                        source_line: child.start_position().row as u32,
                    });
                }
            }
            "member_call_expression" => {
                if let (Some(obj), Some(name)) = (
                    child_by_field(&child, "object"),
                    child_by_field(&child, "name"),
                ) {
                    let obj_text = node_text(source, &obj);
                    let method = node_text(source, &name);
                    let target = if obj_text == "$this" {
                        match container_qualified {
                            Some(c) => format!("{c}.{method}"),
                            None => format!("$this.{method}"),
                        }
                    } else {
                        format!("{obj_text}.{method}")
                    };
                    let edge_kind = if args_are_callable_syntax(source, &child) {
                        EdgeKind::References
                    } else {
                        EdgeKind::Calls
                    };
                    raw_edges.push(RawEdge {
                        source: caller_id,
                        kind: edge_kind,
                        target_name: target,
                        target_module: None,
                        source_line: child.start_position().row as u32,
                    });
                }
            }
            "scoped_call_expression" => {
                if let (Some(scope), Some(name)) = (
                    child_by_field(&child, "scope"),
                    child_by_field(&child, "name"),
                ) {
                    let scope_text = node_text(source, &scope);
                    let method = node_text(source, &name);
                    let target = if matches!(scope_text, "self" | "static") {
                        match container_qualified {
                            Some(c) => format!("{c}.{method}"),
                            None => format!("{scope_text}.{method}"),
                        }
                    } else {
                        format!("{scope_text}.{method}")
                    };
                    let edge_kind = if args_are_callable_syntax(source, &child) {
                        EdgeKind::References
                    } else {
                        EdgeKind::Calls
                    };
                    raw_edges.push(RawEdge {
                        source: caller_id,
                        kind: edge_kind,
                        target_name: target,
                        target_module: None,
                        source_line: child.start_position().row as u32,
                    });
                }
            }
            "arrow_function" => {
                extract_arrow_function(
                    source, &child, file_path, caller_id, container_qualified,
                    symbols, raw_edges,
                );
                continue; // extract_arrow_function handles the body
            }
            "object_creation_expression" => {
                if let Some(anon) = find_child_by_kind(&child, "anonymous_class") {
                    extract_anonymous_class(
                        source, &anon, file_path, caller_id,
                        symbols, raw_edges,
                    );
                }
                extract_calls(source, &child, caller_id, container_qualified, file_path, symbols, raw_edges);
                continue;
            }
            _ => {}
        }
        extract_calls(source, &child, caller_id, container_qualified, file_path, symbols, raw_edges);
    }
}

/// Returns `true` when arguments contain only `variadic_placeholder` (`...`) —
/// the PHP 8.1 first-class callable syntax (`strlen(...)`).
fn args_are_callable_syntax(_source: &[u8], call_node: &tree_sitter::Node) -> bool {
    let args = match child_by_field(call_node, "arguments") {
        Some(a) => a,
        None => return false,
    };
    let mut cursor = args.walk();
    let named_children: Vec<_> = args.children(&mut cursor).filter(|n| n.is_named()).collect();
    // Must be exactly one named child and that child is `variadic_placeholder`.
    named_children.len() == 1 && named_children[0].kind() == "variadic_placeholder"
}

/// Extract a PHP 7.4+ arrow function (`arrow_function` node).
/// Emits a Function symbol `__arrow_<basename>_L<line>`; recurses into body for call edges.
fn extract_arrow_function(
    source: &[u8],
    node: &tree_sitter::Node,
    file_path: &Path,
    parent_id: NodeId,
    container_qualified: Option<&str>,
    symbols: &mut Vec<SymbolNode>,
    raw_edges: &mut Vec<RawEdge>,
) {
    let line = node.start_position().row as u32;
    let basename = file_path.file_stem().unwrap_or_default().to_string_lossy();
    let synthetic_name = format!("__arrow_{basename}_L{line}");

    let params = child_by_field(node, "parameters")
        .map(|p| node_text(source, &p).to_string())
        .unwrap_or_else(|| "()".to_string());

    let id = NodeId::new(
        &file_path.to_string_lossy(),
        &synthetic_name,
        SymbolKind::Function,
        line,
    );

    symbols.push(SymbolNode {
        id,
        name: synthetic_name,
        kind: SymbolKind::Function,
        file_path: file_path.to_path_buf(),
        line_range: (line, node.end_position().row as u32),
        signature: Some(format!("fn{params} =>")),
        doc_comment: None,
        visibility: Visibility::Public,
        language: Language::Php,
        parent: Some(parent_id),
    });

    // Body is the named non-formal_parameters child that follows `=>`.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.is_named() && child.kind() != "formal_parameters" {
            extract_calls(source, &child, id, container_qualified, file_path, symbols, raw_edges);
        }
    }
}

/// Extract an anonymous class (`anonymous_class` inside `object_creation_expression`).
/// Emits a Class symbol `__AnonClass_<basename>_L<line>` with extends/implements edges.
fn extract_anonymous_class(
    source: &[u8],
    node: &tree_sitter::Node,   // the `anonymous_class` node
    file_path: &Path,
    parent_id: NodeId,
    symbols: &mut Vec<SymbolNode>,
    raw_edges: &mut Vec<RawEdge>,
) {
    let line = node.start_position().row as u32;
    let basename = file_path.file_stem().unwrap_or_default().to_string_lossy();
    let synthetic_name = format!("__AnonClass_{basename}_L{line}");

    let id = NodeId::new(
        &file_path.to_string_lossy(),
        &synthetic_name,
        SymbolKind::Class,
        line,
    );

    symbols.push(SymbolNode {
        id,
        name: synthetic_name.clone(),
        kind: SymbolKind::Class,
        file_path: file_path.to_path_buf(),
        line_range: (line, node.end_position().row as u32),
        signature: Some("new class".to_string()),
        doc_comment: None,
        visibility: Visibility::Public,
        language: Language::Php,
        parent: Some(parent_id),
    });

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "base_clause" => {
                emit_inheritance_edges(source, &child, id, EdgeKind::Extends, raw_edges);
            }
            "class_interface_clause" => {
                emit_inheritance_edges(source, &child, id, EdgeKind::Implements, raw_edges);
            }
            _ => {}
        }
    }

    if let Some(body) = find_child_by_kind(node, "declaration_list") {
        walk_class_body(source, &body, file_path, id, &synthetic_name, symbols, raw_edges);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extractor::LanguageExtractor;

    fn extract(src: &str) -> (Vec<SymbolNode>, Vec<RawEdge>) {
        let extractor = PhpExtractor;
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&extractor.tree_sitter_language())
            .expect("tree-sitter-php load failed");
        let tree = parser.parse(src.as_bytes(), None).expect("parse failed");
        let r = extractor.extract(src.as_bytes(), &tree, Path::new("test.php"));
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

    #[test]
    fn extracts_top_level_function() {
        let src = "<?php\nfunction hello() {}\n";
        let (syms, _) = extract(src);
        let f = find(&syms, "hello").expect("hello missing");
        assert_eq!(f.kind, SymbolKind::Function);
        assert_eq!(f.language, Language::Php);
    }

    #[test]
    fn fully_qualifies_class_in_namespace() {
        let src = "<?php\nnamespace App\\Services;\nclass UserService {}\n";
        let (syms, _) = extract(src);
        let c = find(&syms, "App\\Services\\UserService").expect("qualified class missing");
        assert_eq!(c.kind, SymbolKind::Class);
    }

    #[test]
    fn fully_qualifies_function_in_namespace() {
        let src = "<?php\nnamespace App\\Util;\nfunction helper() {}\n";
        let (syms, _) = extract(src);
        assert!(find(&syms, "App\\Util\\helper").is_some());
    }

    #[test]
    fn extracts_method_qualified_with_namespace_and_class() {
        let src = "<?php\nnamespace App;\nclass Foo {\n  public function bar() {}\n}\n";
        let (syms, _) = extract(src);
        let m = find(&syms, "App\\Foo.bar").expect("App\\Foo.bar missing");
        assert_eq!(m.kind, SymbolKind::Method);
        assert_eq!(m.visibility, Visibility::Public);
    }

    #[test]
    fn private_method_visibility() {
        let src = "<?php\nclass Foo {\n  private function secret() {}\n}\n";
        let (syms, _) = extract(src);
        let m = find(&syms, "Foo.secret").expect("Foo.secret missing");
        assert_eq!(m.visibility, Visibility::Private);
    }

    #[test]
    fn protected_method_visibility() {
        let src = "<?php\nclass Foo {\n  protected function helper() {}\n}\n";
        let (syms, _) = extract(src);
        assert_eq!(find(&syms, "Foo.helper").unwrap().visibility, Visibility::Protected);
    }

    #[test]
    fn construct_is_constructor() {
        let src = "<?php\nclass Foo {\n  public function __construct() {}\n}\n";
        let (syms, _) = extract(src);
        assert_eq!(find(&syms, "Foo.__construct").unwrap().kind, SymbolKind::Constructor);
    }

    #[test]
    fn extracts_interface_and_trait() {
        let src = "<?php\ninterface IFoo {}\ntrait Bar {}\n";
        let (syms, _) = extract(src);
        assert_eq!(find(&syms, "IFoo").unwrap().kind, SymbolKind::Interface);
        assert_eq!(find(&syms, "Bar").unwrap().kind, SymbolKind::Trait);
    }

    #[test]
    fn extends_emits_edge() {
        let src = "<?php\nclass Dog extends Animal {}\n";
        let (syms, edges) = extract(src);
        let d = find(&syms, "Dog").unwrap();
        assert!(edges.iter().any(|e|
            e.source == d.id && e.kind == EdgeKind::Extends && e.target_name == "Animal"
        ));
    }

    #[test]
    fn implements_emits_edges() {
        let src = "<?php\nclass Dog implements Animal, Mammal {}\n";
        let (syms, edges) = extract(src);
        let d = find(&syms, "Dog").unwrap();
        let impls: Vec<_> = edges.iter()
            .filter(|e| e.source == d.id && e.kind == EdgeKind::Implements)
            .map(|e| e.target_name.as_str()).collect();
        assert!(impls.contains(&"Animal"));
        assert!(impls.contains(&"Mammal"));
    }

    #[test]
    fn use_trait_in_class_emits_implements() {
        let src = "<?php\nclass Foo {\n  use Bar;\n}\n";
        let (syms, edges) = extract(src);
        let foo = find(&syms, "Foo").unwrap();
        assert!(edges.iter().any(|e|
            e.source == foo.id && e.kind == EdgeKind::Implements && e.target_name == "Bar"
        ));
    }

    #[test]
    fn extracts_use_declaration_as_import() {
        let src = "<?php\nuse App\\Models\\User;\n";
        let (syms, edges) = extract(src);
        assert!(find(&syms, "App\\Models\\User").is_some());
        assert!(edges.iter().any(|e|
            e.kind == EdgeKind::Imports && e.target_name == "App\\Models\\User"
        ));
    }

    #[test]
    fn extracts_grouped_use_declaration() {
        let src = "<?php\nuse App\\Models\\{User, Post};\n";
        let (_, edges) = extract(src);
        let imports: Vec<_> = edges.iter()
            .filter(|e| e.kind == EdgeKind::Imports)
            .map(|e| e.target_name.as_str()).collect();
        assert!(imports.contains(&"App\\Models\\User"), "got: {imports:?}");
        assert!(imports.contains(&"App\\Models\\Post"), "got: {imports:?}");
    }

    #[test]
    fn extracts_property_with_visibility() {
        let src = "<?php\nclass Foo {\n  private $count = 0;\n  public $name;\n}\n";
        let (syms, _) = extract(src);
        let count = find(&syms, "Foo.count").expect("Foo.count missing");
        assert_eq!(count.kind, SymbolKind::Property);
        assert_eq!(count.visibility, Visibility::Private);
        assert_eq!(find(&syms, "Foo.name").unwrap().visibility, Visibility::Public);
    }

    #[test]
    fn extracts_class_constant() {
        let src = "<?php\nclass Foo {\n  const MAX = 100;\n}\n";
        let (syms, _) = extract(src);
        let c = find(&syms, "Foo::MAX").expect("Foo::MAX missing");
        assert_eq!(c.kind, SymbolKind::Constant);
    }

    #[test]
    fn extracts_enum_with_cases() {
        let src = "<?php\nenum Status { case Active; case Inactive; }\n";
        let (syms, _) = extract(src);
        assert_eq!(find(&syms, "Status").unwrap().kind, SymbolKind::Enum);
        assert_eq!(find(&syms, "Status.Active").unwrap().kind, SymbolKind::EnumVariant);
        assert_eq!(find(&syms, "Status.Inactive").unwrap().kind, SymbolKind::EnumVariant);
    }

    #[test]
    fn this_call_qualifies_to_full_container() {
        let src = "<?php\nnamespace App;\nclass Foo {\n  public function a() { $this->b(); }\n  public function b() {}\n}\n";
        let (syms, edges) = extract(src);
        let a = find(&syms, "App\\Foo.a").unwrap();
        let targets = calls_from(&edges, a.id);
        assert!(targets.contains(&"App\\Foo.b"), "got: {targets:?}");
    }

    #[test]
    fn self_static_call_qualifies_to_container() {
        let src = "<?php\nclass Foo {\n  public function a() { self::b(); static::c(); }\n  public static function b() {}\n  public static function c() {}\n}\n";
        let (syms, edges) = extract(src);
        let a = find(&syms, "Foo.a").unwrap();
        let targets = calls_from(&edges, a.id);
        assert!(targets.contains(&"Foo.b"), "got: {targets:?}");
        assert!(targets.contains(&"Foo.c"), "got: {targets:?}");
    }

    #[test]
    fn external_static_call_preserved() {
        let src = "<?php\nclass Foo {\n  public function a() { Logger::info(\"x\"); }\n}\n";
        let (syms, edges) = extract(src);
        let a = find(&syms, "Foo.a").unwrap();
        let targets = calls_from(&edges, a.id);
        assert!(targets.contains(&"Logger.info"), "got: {targets:?}");
    }

    #[test]
    fn extracts_namespace_symbol() {
        let src = "<?php\nnamespace App\\Foo;\n";
        let (syms, _) = extract(src);
        let ns = find(&syms, "App\\Foo").expect("namespace missing");
        assert_eq!(ns.kind, SymbolKind::Namespace);
    }

    #[test]
    fn extracts_doc_comment_on_class() {
        let src = "<?php\n/**\n * A foo widget.\n */\nclass Foo {}\n";
        let (syms, _) = extract(src);
        let c = find(&syms, "Foo").unwrap();
        let doc = c.doc_comment.as_deref().unwrap_or("");
        assert!(doc.contains("A foo widget."), "doc was: {doc:?}");
    }

    // ── New feature tests ────────────────────────────────────────────────────

    #[test]
    fn arrow_function_symbol_and_call_edges() {
        // Symbol emitted with __arrow_ prefix and _L line marker.
        let src1 = "<?php\n$f = fn($x) => $x * 2;\n";
        let (syms1, _) = extract(src1);
        let arrow = syms1.iter().find(|s| s.name.starts_with("__arrow_"))
            .expect("expected __arrow_ symbol");
        assert_eq!(arrow.kind, SymbolKind::Function);
        assert!(arrow.name.contains("_L"), "name missing _L: {}", arrow.name);

        // Outer function still sees array_map as a CALLS edge when arrow fn is an arg.
        let src2 = "<?php\nfunction outer() {\n  $xs = array_map(fn($x) => $x * 2, $arr);\n}\n";
        let (syms2, edges2) = extract(src2);
        let outer = syms2.iter().find(|s| s.name == "outer").expect("outer missing");
        let calls: Vec<_> = edges2.iter()
            .filter(|e| e.source == outer.id && e.kind == EdgeKind::Calls)
            .map(|e| e.target_name.as_str()).collect();
        assert!(calls.contains(&"array_map"), "got: {calls:?}");
    }

    #[test]
    fn anonymous_class_symbol_edges_and_methods() {
        let src = "<?php\n$x = new class extends Base implements I { public function foo() {} };\n";
        let (syms, edges) = extract(src);
        let anon = syms.iter().find(|s| s.name.starts_with("__AnonClass_"))
            .expect("__AnonClass_ missing");
        assert_eq!(anon.kind, SymbolKind::Class);
        assert!(edges.iter().any(|e|
            e.source == anon.id && e.kind == EdgeKind::Extends && e.target_name == "Base"
        ), "expected Extends edge to Base");
        assert!(edges.iter().any(|e|
            e.source == anon.id && e.kind == EdgeKind::Implements && e.target_name == "I"
        ), "expected Implements edge to I");
        // Method inside the anonymous class is qualified to it.
        assert!(syms.iter().any(|s| s.name.contains(".foo") && s.kind == SymbolKind::Method),
            "expected .foo method, got: {:?}", syms.iter().map(|s| &s.name).collect::<Vec<_>>());
    }

    #[test]
    fn first_class_callable_bare_function_emits_references() {
        // `strlen(...)` — first-class callable, not an invocation.
        let src = "<?php\nfunction outer() { $f = strlen(...); }\n";
        let (syms, edges) = extract(src);
        let outer = syms.iter().find(|s| s.name == "outer").expect("outer missing");
        let refs: Vec<_> = edges.iter()
            .filter(|e| e.source == outer.id && e.kind == EdgeKind::References)
            .map(|e| e.target_name.as_str())
            .collect();
        assert!(refs.contains(&"strlen"), "expected References edge to strlen, got: {refs:?}");
        // Must NOT emit a Calls edge for this
        let calls: Vec<_> = edges.iter()
            .filter(|e| e.source == outer.id && e.kind == EdgeKind::Calls && e.target_name == "strlen")
            .collect();
        assert!(calls.is_empty(), "should not have Calls edge to strlen, got: {calls:?}");
    }

    #[test]
    fn first_class_callable_method_emits_references() {
        // `$this->method(...)` — first-class callable reference.
        let src = "<?php\nclass Foo {\n  public function bar() { $f = $this->method(...); }\n  public function method() {}\n}\n";
        let (syms, edges) = extract(src);
        let bar = syms.iter().find(|s| s.name == "Foo.bar").expect("Foo.bar missing");
        let refs: Vec<_> = edges.iter()
            .filter(|e| e.source == bar.id && e.kind == EdgeKind::References)
            .map(|e| e.target_name.as_str())
            .collect();
        assert!(refs.contains(&"Foo.method"), "expected References edge to Foo.method, got: {refs:?}");
    }

    #[test]
    fn regular_call_is_not_references() {
        // `strlen($x)` is a regular call — must stay as Calls, not References.
        let src = "<?php\nfunction outer() { $n = strlen($x); }\n";
        let (syms, edges) = extract(src);
        let outer = syms.iter().find(|s| s.name == "outer").expect("outer missing");
        let calls: Vec<_> = edges.iter()
            .filter(|e| e.source == outer.id && e.kind == EdgeKind::Calls)
            .map(|e| e.target_name.as_str())
            .collect();
        assert!(calls.contains(&"strlen"), "expected Calls edge to strlen, got: {calls:?}");
    }

    #[test]
    fn full_file_smoke() {
        let src = r#"<?php
namespace App\Services;

use App\Models\User;
use Psr\Log\LoggerInterface;

/**
 * User service.
 */
class UserService implements ServiceInterface
{
    private LoggerInterface $logger;

    public function __construct(LoggerInterface $logger)
    {
        $this->logger = $logger;
    }

    public function login(User $user): bool
    {
        $this->log("login");
        return self::isValid($user);
    }

    private function log(string $msg): void
    {
        $this->logger->info($msg);
    }

    public static function isValid(User $u): bool
    {
        return true;
    }
}
"#;
        let (syms, edges) = extract(src);

        // namespace + class + methods qualified
        assert!(find(&syms, "App\\Services").is_some());
        assert!(find(&syms, "App\\Services\\UserService").is_some());
        assert_eq!(
            find(&syms, "App\\Services\\UserService.__construct").unwrap().kind,
            SymbolKind::Constructor
        );
        let login = find(&syms, "App\\Services\\UserService.login").unwrap();
        assert_eq!(login.visibility, Visibility::Public);
        let log = find(&syms, "App\\Services\\UserService.log").unwrap();
        assert_eq!(log.visibility, Visibility::Private);

        // use declarations as imports
        let imports: Vec<_> = edges.iter()
            .filter(|e| e.kind == EdgeKind::Imports)
            .map(|e| e.target_name.as_str()).collect();
        assert!(imports.contains(&"App\\Models\\User"), "got: {imports:?}");
        assert!(imports.contains(&"Psr\\Log\\LoggerInterface"), "got: {imports:?}");

        // implements edge
        let us = find(&syms, "App\\Services\\UserService").unwrap();
        assert!(edges.iter().any(|e|
            e.source == us.id && e.kind == EdgeKind::Implements && e.target_name == "ServiceInterface"
        ));

        // self::isValid → fully qualified container call
        let login_calls = calls_from(&edges, login.id);
        assert!(login_calls.contains(&"App\\Services\\UserService.isValid"),
            "expected fully-qualified self call, got: {login_calls:?}");
        assert!(login_calls.contains(&"App\\Services\\UserService.log"),
            "expected $this->log qualified, got: {login_calls:?}");
    }
}
