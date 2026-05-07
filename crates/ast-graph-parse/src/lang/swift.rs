use ast_graph_core::*;
use crate::extractor::*;
use std::path::Path;

pub struct SwiftExtractor;

impl LanguageExtractor for SwiftExtractor {
    fn language(&self) -> Language {
        Language::Swift
    }

    fn file_extensions(&self) -> &[&str] {
        &["swift"]
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_swift::LANGUAGE.into()
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
            language: Language::Swift,
            parent: None,
        });

        walk_top(source, &tree.root_node(), file_path, file_node_id, &mut symbols, &mut raw_edges);
        ExtractResult { symbols, raw_edges }
    }
}

/// Walk source_file children.
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
            "import_declaration" => {
                extract_import(source, &child, file_path, parent_id, symbols, raw_edges);
            }
            "class_declaration" => {
                extract_class_decl(source, &child, file_path, parent_id, symbols, raw_edges);
            }
            "protocol_declaration" => {
                extract_protocol(source, &child, file_path, parent_id, symbols, raw_edges);
            }
            "function_declaration" => {
                extract_function(source, &child, file_path, parent_id, None, symbols, raw_edges);
            }
            "property_declaration" => {
                extract_property(source, &child, file_path, parent_id, None, symbols, raw_edges);
            }
            "typealias_declaration" => {
                extract_typealias(source, &child, file_path, parent_id, symbols);
            }
            _ => {}
        }
    }
}

/// `import Foo` / `import Foo.Bar` → IMPORTS edge.
fn extract_import(
    source: &[u8],
    node: &tree_sitter::Node,
    file_path: &Path,
    parent_id: NodeId,
    symbols: &mut Vec<SymbolNode>,
    raw_edges: &mut Vec<RawEdge>,
) {
    // Find the imported identifier — it appears as the last named non-modifier child.
    let mut path: Option<String> = None;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() {
            continue;
        }
        if matches!(child.kind(), "modifiers" | "attribute" | "comment" | "multiline_comment") {
            continue;
        }
        if matches!(child.kind(), "identifier" | "simple_identifier" | "navigation_expression") {
            path = Some(node_text(source, &child).to_string());
        }
    }
    let Some(path) = path else { return };

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
        signature: Some(format!("import {path}")),
        doc_comment: None,
        visibility: Visibility::Public,
        language: Language::Swift,
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

/// `class_declaration` covers class / struct / actor / enum / extension —
/// distinguished by the `declaration_kind` field.
fn extract_class_decl(
    source: &[u8],
    node: &tree_sitter::Node,
    file_path: &Path,
    parent_id: NodeId,
    symbols: &mut Vec<SymbolNode>,
    raw_edges: &mut Vec<RawEdge>,
) {
    let kind_text = child_by_field(node, "declaration_kind")
        .map(|n| node_text(source, &n).to_string())
        .unwrap_or_else(|| "class".to_string());
    let name_node = match child_by_field(node, "name") {
        Some(n) => n,
        None => return,
    };
    let name = node_text(source, &name_node).to_string();

    let visibility = read_visibility(source, node);

    // For extension: don't emit a new type symbol — just walk the body and
    // qualify methods to the extended type.  Inheritance specifiers on the
    // extension still produce IMPLEMENTS edges sourced at the extended type.
    if kind_text == "extension" {
        let extended_id = NodeId::new(
            &file_path.to_string_lossy(),
            &name,
            SymbolKind::Class,
            // Use line 0 to share the id with any existing class definition
            // in the same file. If the class is in a different file the
            // resolver will fall back to name-based matching.
            0,
        );
        emit_inheritance(source, node, extended_id, EdgeKind::Implements, raw_edges);
        if let Some(body) = child_by_field(node, "body") {
            walk_type_body(source, &body, file_path, extended_id, &name, symbols, raw_edges);
        }
        return;
    }

    let symbol_kind = match kind_text.as_str() {
        "struct" => SymbolKind::Struct,
        "enum" => SymbolKind::Enum,
        // Actor is closer to a class (reference type with methods); collapse.
        _ => SymbolKind::Class,
    };

    let id = NodeId::new(
        &file_path.to_string_lossy(),
        &name,
        symbol_kind,
        node.start_position().row as u32,
    );

    symbols.push(SymbolNode {
        id,
        name: name.clone(),
        kind: symbol_kind,
        file_path: file_path.to_path_buf(),
        line_range: (node.start_position().row as u32, node.end_position().row as u32),
        signature: Some(format!("{kind_text} {name}")),
        doc_comment: extract_preceding_doc_comment(source, node),
        visibility,
        language: Language::Swift,
        parent: Some(parent_id),
    });

    // `class Foo: Base, P1, P2` — first inheritance specifier is the
    // superclass, subsequent ones are protocol conformance. Swift doesn't
    // syntactically distinguish the two, so emit EXTENDS for the first
    // type-identifier and IMPLEMENTS for the rest.
    emit_class_inheritance(source, node, id, raw_edges);

    if let Some(body) = child_by_field(node, "body") {
        walk_type_body(source, &body, file_path, id, &name, symbols, raw_edges);
    }
}

fn extract_protocol(
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
    let visibility = read_visibility(source, node);

    let id = NodeId::new(
        &file_path.to_string_lossy(),
        &name,
        SymbolKind::Interface,
        node.start_position().row as u32,
    );

    symbols.push(SymbolNode {
        id,
        name: name.clone(),
        kind: SymbolKind::Interface,
        file_path: file_path.to_path_buf(),
        line_range: (node.start_position().row as u32, node.end_position().row as u32),
        signature: Some(format!("protocol {name}")),
        doc_comment: extract_preceding_doc_comment(source, node),
        visibility,
        language: Language::Swift,
        parent: Some(parent_id),
    });

    // Protocol-of-protocol conformance: all are IMPLEMENTS.
    emit_inheritance(source, node, id, EdgeKind::Implements, raw_edges);

    if let Some(body) = child_by_field(node, "body") {
        walk_type_body(source, &body, file_path, id, &name, symbols, raw_edges);
    }
}

/// Walk the body of a class/struct/enum/protocol/extension, extracting
/// methods, init, properties, nested types, enum cases.
fn walk_type_body(
    source: &[u8],
    body: &tree_sitter::Node,
    file_path: &Path,
    parent_id: NodeId,
    container_name: &str,
    symbols: &mut Vec<SymbolNode>,
    raw_edges: &mut Vec<RawEdge>,
) {
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        match child.kind() {
            "function_declaration" | "protocol_function_declaration" => {
                extract_function(source, &child, file_path, parent_id, Some(container_name), symbols, raw_edges);
            }
            "init_declaration" => {
                extract_init(source, &child, file_path, parent_id, container_name, symbols, raw_edges);
            }
            "property_declaration" | "protocol_property_declaration" => {
                extract_property(source, &child, file_path, parent_id, Some(container_name), symbols, raw_edges);
            }
            "subscript_declaration" => {
                extract_subscript(source, &child, file_path, parent_id, container_name, symbols, raw_edges);
            }
            "class_declaration" => {
                // Nested type — recurse.
                extract_class_decl(source, &child, file_path, parent_id, symbols, raw_edges);
            }
            "protocol_declaration" => {
                extract_protocol(source, &child, file_path, parent_id, symbols, raw_edges);
            }
            "enum_entry" => {
                extract_enum_entry(source, &child, file_path, parent_id, container_name, symbols);
            }
            "typealias_declaration" => {
                extract_typealias(source, &child, file_path, parent_id, symbols);
            }
            _ => {}
        }
    }
}

fn extract_function(
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

    let return_type = child_by_field(node, "return_type")
        .map(|r| format!(" -> {}", node_text(source, &r)))
        .unwrap_or_default();

    let visibility = read_visibility(source, node);

    let (kind, qualified) = match container_name {
        Some(cn) => (SymbolKind::Method, format!("{cn}.{name}")),
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
        name: qualified,
        kind,
        file_path: file_path.to_path_buf(),
        line_range: (node.start_position().row as u32, node.end_position().row as u32),
        signature: Some(format!("func {name}{return_type}")),
        doc_comment: extract_preceding_doc_comment(source, node),
        visibility,
        language: Language::Swift,
        parent: Some(parent_id),
    });

    if let Some(body) = child_by_field(node, "body") {
        // Collect generic type parameters to annotate call targets for disambiguation.
        let generic_params = collect_generic_params(source, node);
        extract_calls_with_generics(source, &body, id, container_name, &generic_params, raw_edges);
    }
}

fn extract_init(
    source: &[u8],
    node: &tree_sitter::Node,
    file_path: &Path,
    parent_id: NodeId,
    container_name: &str,
    symbols: &mut Vec<SymbolNode>,
    raw_edges: &mut Vec<RawEdge>,
) {
    let visibility = read_visibility(source, node);
    let qualified = format!("{container_name}.init");

    let id = NodeId::new(
        &file_path.to_string_lossy(),
        &qualified,
        SymbolKind::Constructor,
        node.start_position().row as u32,
    );

    symbols.push(SymbolNode {
        id,
        name: qualified,
        kind: SymbolKind::Constructor,
        file_path: file_path.to_path_buf(),
        line_range: (node.start_position().row as u32, node.end_position().row as u32),
        signature: Some("init".to_string()),
        doc_comment: extract_preceding_doc_comment(source, node),
        visibility,
        language: Language::Swift,
        parent: Some(parent_id),
    });

    if let Some(body) = child_by_field(node, "body") {
        extract_calls(source, &body, id, Some(container_name), raw_edges);
    }
}

fn extract_property(
    source: &[u8],
    node: &tree_sitter::Node,
    file_path: &Path,
    parent_id: NodeId,
    container_name: Option<&str>,
    symbols: &mut Vec<SymbolNode>,
    raw_edges: &mut Vec<RawEdge>,
) {
    let visibility = read_visibility(source, node);
    let name_node = match child_by_field(node, "name") {
        Some(n) => n,
        None => return,
    };
    // Name is a pattern; for the common single-identifier case, the text is
    // the variable name (with no `var`/`let` keyword).
    let raw = node_text(source, &name_node).to_string();
    // Strip any leading binding keyword if it appears (defensive).
    let bare = raw
        .trim_start_matches("var ")
        .trim_start_matches("let ")
        .trim()
        .to_string();
    if bare.is_empty() {
        return;
    }

    let qualified = match container_name {
        Some(cn) => format!("{cn}.{bare}"),
        None => bare.clone(),
    };

    // Detect computed property: property_declaration with a computed_property child
    // that contains getter/setter blocks or is a bare shorthand getter.
    let is_computed = find_child_by_kind(node, "computed_property").is_some();

    let sig_prefix = if is_computed { "computed var" } else { "var" };

    let id = NodeId::new(
        &file_path.to_string_lossy(),
        &qualified,
        SymbolKind::Property,
        node.start_position().row as u32,
    );

    symbols.push(SymbolNode {
        id,
        name: qualified,
        kind: SymbolKind::Property,
        file_path: file_path.to_path_buf(),
        line_range: (node.start_position().row as u32, node.end_position().row as u32),
        signature: Some(format!("{sig_prefix} {bare}")),
        doc_comment: extract_preceding_doc_comment(source, node),
        visibility,
        language: Language::Swift,
        parent: Some(parent_id),
    });

    // For computed properties, extract CALLS edges from getter and setter bodies.
    if is_computed {
        if let Some(computed) = find_child_by_kind(node, "computed_property") {
            extract_calls_from_computed_property(source, &computed, id, container_name, raw_edges);
        }
    }
}

/// Walk a `computed_property` node and extract CALLS from getter/setter statement blocks.
/// Handles two shapes:
///   - `var foo { <statements> }` — shorthand getter (statements directly in computed_property)
///   - `var foo { get { ... } set { ... } }` — explicit getter/setter via computed_getter/computed_setter
fn extract_calls_from_computed_property(
    source: &[u8],
    computed: &tree_sitter::Node,
    caller_id: NodeId,
    container_name: Option<&str>,
    raw_edges: &mut Vec<RawEdge>,
) {
    let mut cursor = computed.walk();
    let mut has_explicit_accessor = false;
    for child in computed.children(&mut cursor) {
        match child.kind() {
            "computed_getter" | "computed_setter" => {
                has_explicit_accessor = true;
                // Body is the statements node inside the accessor block.
                if let Some(stmts) = find_child_by_kind(&child, "statements") {
                    extract_calls(source, &stmts, caller_id, container_name, raw_edges);
                }
            }
            _ => {}
        }
    }
    // Shorthand getter: no explicit accessor nodes — statements live directly in computed_property.
    if !has_explicit_accessor {
        if let Some(stmts) = find_child_by_kind(computed, "statements") {
            extract_calls(source, &stmts, caller_id, container_name, raw_edges);
        }
    }
}

fn extract_enum_entry(
    source: &[u8],
    node: &tree_sitter::Node,
    file_path: &Path,
    parent_id: NodeId,
    container_name: &str,
    symbols: &mut Vec<SymbolNode>,
) {
    let name_node = match child_by_field(node, "name") {
        Some(n) => n,
        None => return,
    };
    let bare = node_text(source, &name_node).to_string();
    let qualified = format!("{container_name}.{bare}");
    let id = NodeId::new(
        &file_path.to_string_lossy(),
        &qualified,
        SymbolKind::EnumVariant,
        node.start_position().row as u32,
    );
    symbols.push(SymbolNode {
        id,
        name: qualified,
        kind: SymbolKind::EnumVariant,
        file_path: file_path.to_path_buf(),
        line_range: (node.start_position().row as u32, node.end_position().row as u32),
        signature: Some(format!("case {bare}")),
        doc_comment: None,
        visibility: Visibility::Public,
        language: Language::Swift,
        parent: Some(parent_id),
    });
}

/// `subscript_declaration` — emits as Method with the special name `__subscript`
/// qualified to the enclosing type, e.g. `Array.__subscript`.
/// Parameters are `parameter` children of the subscript node (before the return type).
fn extract_subscript(
    source: &[u8],
    node: &tree_sitter::Node,
    file_path: &Path,
    parent_id: NodeId,
    container_name: &str,
    symbols: &mut Vec<SymbolNode>,
    raw_edges: &mut Vec<RawEdge>,
) {
    let visibility = read_visibility(source, node);
    let qualified = format!("{container_name}.__subscript");

    // Collect parameter labels for the signature string.
    let mut params: Vec<String> = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "parameter" {
            params.push(node_text(source, &child).to_string());
        }
    }

    let return_type = child_by_field(node, "return_type")
        .map(|r| format!(" -> {}", node_text(source, &r)))
        .unwrap_or_default();

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
        signature: Some(format!("subscript({}){}", params.join(", "), return_type)),
        doc_comment: extract_preceding_doc_comment(source, node),
        visibility,
        language: Language::Swift,
        parent: Some(parent_id),
    });

    // Extract CALLS from the subscript body (a computed_property node).
    if let Some(body) = find_child_by_kind(node, "computed_property") {
        extract_calls_from_computed_property(source, &body, id, Some(container_name), raw_edges);
    }
}

fn extract_typealias(
    source: &[u8],
    node: &tree_sitter::Node,
    file_path: &Path,
    parent_id: NodeId,
    symbols: &mut Vec<SymbolNode>,
) {
    let name_node = match child_by_field(node, "name") {
        Some(n) => n,
        None => return,
    };
    let name = node_text(source, &name_node).to_string();
    let value = child_by_field(node, "value")
        .map(|v| format!(" = {}", node_text(source, &v)))
        .unwrap_or_default();

    let id = NodeId::new(
        &file_path.to_string_lossy(),
        &name,
        SymbolKind::TypeAlias,
        node.start_position().row as u32,
    );
    symbols.push(SymbolNode {
        id,
        name: name.clone(),
        kind: SymbolKind::TypeAlias,
        file_path: file_path.to_path_buf(),
        line_range: (node.start_position().row as u32, node.end_position().row as u32),
        signature: Some(format!("typealias {name}{value}")),
        doc_comment: extract_preceding_doc_comment(source, node),
        visibility: Visibility::Public,
        language: Language::Swift,
        parent: Some(parent_id),
    });
}

/// Find `visibility_modifier` text inside a declaration's `modifiers` block.
/// Maps Swift's 5 levels to our 4-variant enum:
///   open → Public, public → Public, internal → Internal,
///   fileprivate → Private, private → Private.
fn read_visibility(source: &[u8], node: &tree_sitter::Node) -> Visibility {
    let mods = match find_child_by_kind(node, "modifiers") {
        Some(m) => m,
        None => return Visibility::Internal,
    };
    let mut cursor = mods.walk();
    for child in mods.children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            return match node_text(source, &child) {
                "open" | "public" => Visibility::Public,
                "internal" => Visibility::Internal,
                "fileprivate" | "private" => Visibility::Private,
                _ => Visibility::Internal,
            };
        }
    }
    Visibility::Internal
}

/// `class Foo: Base, P1, P2` — first inheritance specifier is the
/// superclass (EXTENDS), the rest are protocol conformance (IMPLEMENTS).
fn emit_class_inheritance(
    source: &[u8],
    node: &tree_sitter::Node,
    source_id: NodeId,
    raw_edges: &mut Vec<RawEdge>,
) {
    let mut cursor = node.walk();
    let mut first = true;
    for child in node.children(&mut cursor) {
        if child.kind() != "inheritance_specifier" {
            continue;
        }
        let name = match child_by_field(&child, "inherits_from") {
            Some(n) => node_text(source, &n).to_string(),
            None => continue,
        };
        let kind = if first { EdgeKind::Extends } else { EdgeKind::Implements };
        first = false;
        raw_edges.push(RawEdge {
            source: source_id,
            kind,
            target_name: name,
            target_module: None,
            source_line: child.start_position().row as u32,
        });
    }
}

fn emit_inheritance(
    source: &[u8],
    node: &tree_sitter::Node,
    source_id: NodeId,
    kind: EdgeKind,
    raw_edges: &mut Vec<RawEdge>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "inheritance_specifier" {
            continue;
        }
        let name = match child_by_field(&child, "inherits_from") {
            Some(n) => node_text(source, &n).to_string(),
            None => continue,
        };
        raw_edges.push(RawEdge {
            source: source_id,
            kind,
            target_name: name,
            target_module: None,
            source_line: child.start_position().row as u32,
        });
    }
}

/// Walk a function/init body collecting CALLS edges. Handles two shapes:
///   - `simple_identifier(...)` → bare call, target = identifier text
///   - `navigation_expression(...)` → method-style call, target = "Recv.name"
/// `self.method` and bare `method` inside a type body get qualified to
/// `Container.method`.
/// `generic_params`: type parameter names from the enclosing function's
/// `type_parameters` clause, used to annotate call targets for resolver disambiguation.
fn extract_calls(
    source: &[u8],
    node: &tree_sitter::Node,
    caller_id: NodeId,
    container_name: Option<&str>,
    raw_edges: &mut Vec<RawEdge>,
) {
    extract_calls_with_generics(source, node, caller_id, container_name, &[], raw_edges);
}

fn extract_calls_with_generics(
    source: &[u8],
    node: &tree_sitter::Node,
    caller_id: NodeId,
    container_name: Option<&str>,
    generic_params: &[String],
    raw_edges: &mut Vec<RawEdge>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "call_expression" {
            if let Some(target) = build_call_target(source, &child, container_name, generic_params) {
                raw_edges.push(RawEdge {
                    source: caller_id,
                    kind: EdgeKind::Calls,
                    target_name: target,
                    target_module: None,
                    source_line: child.start_position().row as u32,
                });
            }
        }
        extract_calls_with_generics(source, &child, caller_id, container_name, generic_params, raw_edges);
    }
}

/// Collect type parameter names from a `type_parameters` node on a function declaration.
/// E.g. `<T: Comparable, U>` → ["T", "U"]
fn collect_generic_params(source: &[u8], func_node: &tree_sitter::Node) -> Vec<String> {
    let type_params = match find_child_by_kind(func_node, "type_parameters") {
        Some(n) => n,
        None => return Vec::new(),
    };
    let mut names = Vec::new();
    let mut cursor = type_params.walk();
    for child in type_params.children(&mut cursor) {
        if child.kind() == "type_parameter" {
            // The first named child is the type identifier (the param name).
            // Use find_child_by_kind to avoid cursor lifetime issues.
            if let Some(name_node) = find_child_by_kind(&child, "type_identifier") {
                names.push(node_text(source, &name_node).to_string());
            }
        }
    }
    names
}

fn build_call_target(
    source: &[u8],
    call: &tree_sitter::Node,
    container_name: Option<&str>,
    generic_params: &[String],
) -> Option<String> {
    // The callable is the first named child of call_expression that isn't a
    // call_suffix (call_suffix is the trailing parenthesized argument list).
    let mut cursor = call.walk();
    let callable = call.children(&mut cursor)
        .find(|n| n.is_named() && n.kind() != "call_suffix")?;

    match callable.kind() {
        "simple_identifier" => {
            let name = node_text(source, &callable);
            // Bare call inside a type body — Swift resolves these to `self`
            // by default, so qualify just like other languages do for
            // unqualified self-calls inside methods.
            if let Some(cn) = container_name {
                Some(format!("{cn}.{name}"))
            } else {
                Some(name.to_string())
            }
        }
        "navigation_expression" => {
            // target.suffix(simple_identifier "name")
            let suffix = child_by_field(&callable, "suffix")?;
            let suffix_id = child_by_field(&suffix, "suffix")?;
            let method = node_text(source, &suffix_id);
            let target_node = child_by_field(&callable, "target")?;
            let target_text = node_text(source, &target_node);
            // `self.foo` → `Container.foo`
            if target_text == "self" {
                if let Some(cn) = container_name {
                    return Some(format!("{cn}.{method}"));
                }
            }
            // Annotate receiver with generic context when the receiver text
            // is a simple identifier that shares a name with a type parameter,
            // helping the resolver disambiguate calls on generic types.
            // E.g. `func f<T>(_ t: T) { t.doThing() }` → "T.doThing" instead of "t.doThing".
            if !generic_params.is_empty() {
                // Check if any generic param name appears in or equals the receiver text.
                // For `[T]`-typed receivers the receiver text is the variable name (e.g. `xs`),
                // so we can only annotate when the receiver IS a type param name itself.
                if generic_params.iter().any(|p| p == target_text) {
                    return Some(format!("{target_text}<{}>.{method}", generic_params.join(", ")));
                }
            }
            Some(format!("{target_text}.{method}"))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extractor::LanguageExtractor;

    fn extract(src: &str) -> (Vec<SymbolNode>, Vec<RawEdge>) {
        let extractor = SwiftExtractor;
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&extractor.tree_sitter_language())
            .expect("tree-sitter-swift load failed");
        let tree = parser.parse(src.as_bytes(), None).expect("parse failed");
        let r = extractor.extract(src.as_bytes(), &tree, Path::new("test.swift"));
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
    fn extracts_class() {
        let (syms, _) = extract("class Foo {}\n");
        let c = find(&syms, "Foo").expect("Foo missing");
        assert_eq!(c.kind, SymbolKind::Class);
        assert_eq!(c.language, Language::Swift);
    }

    #[test]
    fn extracts_struct() {
        let (syms, _) = extract("struct Point {}\n");
        let s = find(&syms, "Point").expect("Point missing");
        assert_eq!(s.kind, SymbolKind::Struct);
    }

    #[test]
    fn extracts_actor_as_class() {
        // No Actor variant in our enum — collapse to Class.
        let (syms, _) = extract("actor Cache {}\n");
        let a = find(&syms, "Cache").expect("Cache missing");
        assert_eq!(a.kind, SymbolKind::Class);
    }

    #[test]
    fn extracts_protocol_as_interface() {
        let (syms, _) = extract("protocol Drawable {}\n");
        let p = find(&syms, "Drawable").expect("Drawable missing");
        assert_eq!(p.kind, SymbolKind::Interface);
    }

    #[test]
    fn class_with_superclass_emits_extends_then_implements() {
        let src = "class Dog: Animal, Pet, Trainable {}\n";
        let (syms, edges) = extract(src);
        let dog = find(&syms, "Dog").expect("Dog missing");

        let extends: Vec<_> = edges.iter()
            .filter(|e| e.source == dog.id && e.kind == EdgeKind::Extends)
            .map(|e| e.target_name.as_str()).collect();
        assert_eq!(extends, vec!["Animal"]);

        let implements: Vec<_> = edges.iter()
            .filter(|e| e.source == dog.id && e.kind == EdgeKind::Implements)
            .map(|e| e.target_name.as_str()).collect();
        assert!(implements.contains(&"Pet"));
        assert!(implements.contains(&"Trainable"));
    }

    #[test]
    fn extracts_method_qualified_with_class() {
        let src = "class Dog {\n  func bark() {}\n}\n";
        let (syms, _) = extract(src);
        let m = find(&syms, "Dog.bark").expect("Dog.bark missing");
        assert_eq!(m.kind, SymbolKind::Method);
    }

    #[test]
    fn extracts_init_as_constructor() {
        let src = "class Dog {\n  init(name: String) {}\n}\n";
        let (syms, _) = extract(src);
        let c = find(&syms, "Dog.init").expect("Dog.init missing");
        assert_eq!(c.kind, SymbolKind::Constructor);
    }

    #[test]
    fn extension_qualifies_methods_to_extended_type() {
        let src = "extension String {\n  func reverseIt() {}\n}\n";
        let (syms, _) = extract(src);
        let m = find(&syms, "String.reverseIt").expect("String.reverseIt missing");
        assert_eq!(m.kind, SymbolKind::Method);
        // Extension itself does NOT emit a separate type symbol.
        assert!(syms.iter().filter(|s| s.name == "String" && s.kind == SymbolKind::Class).count() == 0);
    }

    #[test]
    fn visibility_open_and_public_map_to_public() {
        let src = "open class A {}\npublic class B {}\n";
        let (syms, _) = extract(src);
        assert_eq!(find(&syms, "A").unwrap().visibility, Visibility::Public);
        assert_eq!(find(&syms, "B").unwrap().visibility, Visibility::Public);
    }

    #[test]
    fn visibility_internal_default_and_explicit() {
        let src = "class Default {}\ninternal class Explicit {}\n";
        let (syms, _) = extract(src);
        assert_eq!(find(&syms, "Default").unwrap().visibility, Visibility::Internal);
        assert_eq!(find(&syms, "Explicit").unwrap().visibility, Visibility::Internal);
    }

    #[test]
    fn visibility_private_and_fileprivate_map_to_private() {
        let src = "private class A {}\nfileprivate class B {}\n";
        let (syms, _) = extract(src);
        assert_eq!(find(&syms, "A").unwrap().visibility, Visibility::Private);
        assert_eq!(find(&syms, "B").unwrap().visibility, Visibility::Private);
    }

    #[test]
    fn extracts_property() {
        let src = "class Foo {\n  var count: Int = 0\n}\n";
        let (syms, _) = extract(src);
        let p = find(&syms, "Foo.count").expect("Foo.count missing");
        assert_eq!(p.kind, SymbolKind::Property);
    }

    #[test]
    fn extracts_enum_with_cases() {
        let src = "enum Color {\n  case red\n  case green\n  case blue\n}\n";
        let (syms, _) = extract(src);
        assert_eq!(find(&syms, "Color").unwrap().kind, SymbolKind::Enum);
        assert_eq!(find(&syms, "Color.red").unwrap().kind, SymbolKind::EnumVariant);
        assert_eq!(find(&syms, "Color.green").unwrap().kind, SymbolKind::EnumVariant);
    }

    #[test]
    fn extracts_typealias() {
        let src = "typealias UserId = Int\n";
        let (syms, _) = extract(src);
        assert_eq!(find(&syms, "UserId").unwrap().kind, SymbolKind::TypeAlias);
    }

    #[test]
    fn extracts_top_level_function() {
        let (syms, _) = extract("func greet() {}\n");
        let f = find(&syms, "greet").expect("greet missing");
        assert_eq!(f.kind, SymbolKind::Function);
    }

    #[test]
    fn extracts_import() {
        let src = "import Foundation\n";
        let (syms, edges) = extract(src);
        assert!(find(&syms, "Foundation").is_some());
        assert!(edges.iter().any(|e|
            e.kind == EdgeKind::Imports && e.target_name == "Foundation"
        ));
    }

    #[test]
    fn self_call_qualifies_to_class() {
        let src = "class Foo {\n  func a() { self.b() }\n  func b() {}\n}\n";
        let (syms, edges) = extract(src);
        let a = find(&syms, "Foo.a").unwrap();
        let targets = calls_from(&edges, a.id);
        assert!(targets.contains(&"Foo.b"), "got: {targets:?}");
    }

    #[test]
    fn external_call_preserved() {
        let src = "class Foo {\n  func a() { Logger.log(\"hi\") }\n}\n";
        let (syms, edges) = extract(src);
        let a = find(&syms, "Foo.a").unwrap();
        let targets = calls_from(&edges, a.id);
        assert!(targets.contains(&"Logger.log"), "got: {targets:?}");
    }

    #[test]
    fn extracts_doc_comment_on_class() {
        let src = "/// A foo widget.\nclass Foo {}\n";
        let (syms, _) = extract(src);
        let c = find(&syms, "Foo").unwrap();
        let doc = c.doc_comment.as_deref().unwrap_or("");
        assert!(doc.contains("A foo widget."), "doc was: {doc:?}");
    }

    #[test]
    fn full_file_smoke() {
        let src = r#"import Foundation

/// A user record.
public struct User {
    let id: String
    var name: String

    init(id: String, name: String) {
        self.id = id
        self.name = name
    }

    public func describe() -> String {
        return self.format()
    }

    private func format() -> String {
        return "\(name)#\(id)"
    }
}

protocol Greeter {
    func greet() -> String
}

extension User: Greeter {
    func greet() -> String {
        return "Hi \(name)"
    }
}
"#;
        let (syms, edges) = extract(src);

        // type symbols
        let user = find(&syms, "User").expect("User missing");
        assert_eq!(user.kind, SymbolKind::Struct);
        assert_eq!(user.visibility, Visibility::Public);

        let greeter = find(&syms, "Greeter").expect("Greeter missing");
        assert_eq!(greeter.kind, SymbolKind::Interface);

        // members
        assert_eq!(find(&syms, "User.init").unwrap().kind, SymbolKind::Constructor);
        let describe = find(&syms, "User.describe").unwrap();
        assert_eq!(describe.visibility, Visibility::Public);
        let format = find(&syms, "User.format").unwrap();
        assert_eq!(format.visibility, Visibility::Private);

        // extension method on User
        assert!(find(&syms, "User.greet").is_some(), "extension method missing");

        // self.format() inside describe → User.format
        let calls = calls_from(&edges, describe.id);
        assert!(calls.contains(&"User.format"),
            "expected User.format call, got: {calls:?}");

        // import edge
        assert!(edges.iter().any(|e|
            e.kind == EdgeKind::Imports && e.target_name == "Foundation"
        ));
    }

}
