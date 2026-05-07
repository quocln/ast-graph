/// Integration tests loading real-world-style Swift fixture files.
/// Validates that SwiftExtractor correctly handles SwiftUI views,
/// ObservableObject view models, protocol+generic network services,
/// computed properties, subscript declarations, and CALLS edge emission.
use ast_graph_core::{EdgeKind, SymbolKind, Visibility};
use ast_graph_parse::extractor::LanguageExtractor;
use ast_graph_parse::lang::swift::SwiftExtractor;
use std::path::Path;

fn extract_fixture(rel_path: &str) -> (Vec<ast_graph_core::SymbolNode>, Vec<ast_graph_core::RawEdge>) {
    let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/swift")
        .join(rel_path);
    let source = std::fs::read(&fixture_path)
        .unwrap_or_else(|e| panic!("failed to read fixture {rel_path}: {e}"));
    let extractor = SwiftExtractor;
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&extractor.tree_sitter_language())
        .expect("tree-sitter-swift load failed");
    let tree = parser
        .parse(&source, None)
        .expect("parse failed");
    let result = extractor.extract(&source, &tree, &fixture_path);
    (result.symbols, result.raw_edges)
}

fn find_sym<'a>(syms: &'a [ast_graph_core::SymbolNode], name: &str) -> Option<&'a ast_graph_core::SymbolNode> {
    syms.iter().find(|s| s.name == name)
}

// ---------------------------------------------------------------------------
// ContentView.swift — SwiftUI View with computed property, button calls
// ---------------------------------------------------------------------------

#[test]
fn content_view_struct_extracted() {
    let (syms, _) = extract_fixture("ContentView.swift");
    let s = find_sym(&syms, "ContentView").expect("ContentView missing");
    assert_eq!(s.kind, SymbolKind::Struct);
    assert_eq!(s.visibility, Visibility::Public);
}

#[test]
fn content_view_body_is_computed_property() {
    let (syms, _) = extract_fixture("ContentView.swift");
    let body = find_sym(&syms, "ContentView.body").expect("ContentView.body missing");
    assert_eq!(body.kind, SymbolKind::Property);
    let sig = body.signature.as_deref().unwrap_or("");
    assert!(sig.starts_with("computed var"), "expected computed var, got: {sig:?}");
}

#[test]
fn content_view_header_text_computed_emits_calls() {
    let (syms, edges) = extract_fixture("ContentView.swift");
    let prop = find_sym(&syms, "ContentView.headerText").expect("ContentView.headerText missing");
    let sig = prop.signature.as_deref().unwrap_or("");
    assert!(sig.starts_with("computed var"), "headerText should be computed, got: {sig:?}");
    // headerText calls formatHeader
    let calls: Vec<&str> = edges.iter()
        .filter(|e| e.source == prop.id && e.kind == EdgeKind::Calls)
        .map(|e| e.target_name.as_str())
        .collect();
    assert!(
        calls.iter().any(|c| c.contains("formatHeader")),
        "expected formatHeader call from headerText, got: {calls:?}"
    );
}

#[test]
fn content_view_methods_extracted() {
    let (syms, _) = extract_fixture("ContentView.swift");
    assert!(find_sym(&syms, "ContentView.increment").is_some(), "increment missing");
    assert!(find_sym(&syms, "ContentView.reset").is_some(), "reset missing");
    assert!(find_sym(&syms, "ContentView.logEvent").is_some(), "logEvent missing");
}

#[test]
fn content_view_increment_calls_log_event() {
    let (syms, edges) = extract_fixture("ContentView.swift");
    let inc = find_sym(&syms, "ContentView.increment").expect("increment missing");
    let calls: Vec<&str> = edges.iter()
        .filter(|e| e.source == inc.id && e.kind == EdgeKind::Calls)
        .map(|e| e.target_name.as_str())
        .collect();
    assert!(
        calls.iter().any(|c| c.contains("logEvent")),
        "expected logEvent call from increment, got: {calls:?}"
    );
}

// ---------------------------------------------------------------------------
// UserViewModel.swift — ObservableObject, Combine, async/await
// ---------------------------------------------------------------------------

#[test]
fn user_view_model_class_extracted() {
    let (syms, _) = extract_fixture("UserViewModel.swift");
    let cls = find_sym(&syms, "UserViewModel").expect("UserViewModel missing");
    assert_eq!(cls.kind, SymbolKind::Class);
    assert_eq!(cls.visibility, Visibility::Public);
}

#[test]
fn user_view_model_implements_observable_object() {
    let (_, edges) = extract_fixture("UserViewModel.swift");
    // Swift: first (and only) specifier after `:` emits EXTENDS edge.
    let extends: Vec<&str> = edges.iter()
        .filter(|e| e.kind == EdgeKind::Extends)
        .map(|e| e.target_name.as_str())
        .collect();
    assert!(
        extends.contains(&"ObservableObject"),
        "expected ObservableObject in extends, got: {extends:?}"
    );
}

#[test]
fn user_view_model_has_init_constructor() {
    let (syms, _) = extract_fixture("UserViewModel.swift");
    let init_sym = find_sym(&syms, "UserViewModel.init").expect("UserViewModel.init missing");
    assert_eq!(init_sym.kind, SymbolKind::Constructor);
}

#[test]
fn user_view_model_published_properties_extracted() {
    let (syms, _) = extract_fixture("UserViewModel.swift");
    assert!(find_sym(&syms, "UserViewModel.name").is_some(), "name property missing");
    assert!(find_sym(&syms, "UserViewModel.email").is_some(), "email property missing");
    assert!(find_sym(&syms, "UserViewModel.isLoading").is_some(), "isLoading property missing");
}

#[test]
fn user_view_model_async_methods_extracted() {
    let (syms, _) = extract_fixture("UserViewModel.swift");
    assert!(find_sym(&syms, "UserViewModel.loadUser").is_some(), "loadUser missing");
    assert!(find_sym(&syms, "UserViewModel.saveUser").is_some(), "saveUser missing");
}

#[test]
fn user_view_model_load_user_calls_apply_user() {
    let (syms, edges) = extract_fixture("UserViewModel.swift");
    let load = find_sym(&syms, "UserViewModel.loadUser").expect("loadUser missing");
    let calls: Vec<&str> = edges.iter()
        .filter(|e| e.source == load.id && e.kind == EdgeKind::Calls)
        .map(|e| e.target_name.as_str())
        .collect();
    assert!(
        calls.iter().any(|c| c.contains("applyUser")),
        "expected applyUser call from loadUser, got: {calls:?}"
    );
}

// ---------------------------------------------------------------------------
// NetworkService.swift — protocol + generics + subscript + extensions
// ---------------------------------------------------------------------------

#[test]
fn network_service_protocol_extracted() {
    let (syms, _) = extract_fixture("NetworkService.swift");
    let proto = find_sym(&syms, "NetworkServiceProtocol").expect("NetworkServiceProtocol missing");
    assert_eq!(proto.kind, SymbolKind::Interface);
}

#[test]
fn network_service_class_implements_protocol() {
    let (_, edges) = extract_fixture("NetworkService.swift");
    // Swift: first (and only) specifier after `:` emits EXTENDS edge.
    // The resolver treats it as protocol conformance at the semantic level.
    let extends: Vec<&str> = edges.iter()
        .filter(|e| e.kind == EdgeKind::Extends)
        .map(|e| e.target_name.as_str())
        .collect();
    assert!(
        extends.contains(&"NetworkServiceProtocol"),
        "expected NetworkServiceProtocol in extends, got: {extends:?}"
    );
}

#[test]
fn network_service_init_extracted() {
    let (syms, _) = extract_fixture("NetworkService.swift");
    assert!(find_sym(&syms, "NetworkService.init").is_some(), "NetworkService.init missing");
}

#[test]
fn network_service_extension_subscript_extracted() {
    let (syms, _) = extract_fixture("NetworkService.swift");
    // Extension adds `subscript(key: String) -> Data?` — emitted as NetworkService.__subscript
    let sub = find_sym(&syms, "NetworkService.__subscript").expect("NetworkService.__subscript missing");
    assert_eq!(sub.kind, SymbolKind::Method);
    let sig = sub.signature.as_deref().unwrap_or("");
    assert!(sig.contains("subscript"), "sig was: {sig:?}");
}

#[test]
fn network_service_generic_fetch_extracted() {
    let (syms, _) = extract_fixture("NetworkService.swift");
    assert!(find_sym(&syms, "NetworkService.fetch").is_some(), "fetch missing");
}

#[test]
fn network_service_fetch_calls_load_data_and_decode() {
    let (syms, edges) = extract_fixture("NetworkService.swift");
    let fetch = find_sym(&syms, "NetworkService.fetch").expect("fetch missing");
    let calls: Vec<&str> = edges.iter()
        .filter(|e| e.source == fetch.id && e.kind == EdgeKind::Calls)
        .map(|e| e.target_name.as_str())
        .collect();
    assert!(
        calls.iter().any(|c| c.contains("loadData")),
        "expected loadData call from fetch, got: {calls:?}"
    );
    assert!(
        calls.iter().any(|c| c.contains("decode")),
        "expected decode call from fetch, got: {calls:?}"
    );
}

#[test]
fn network_error_enum_extracted() {
    let (syms, _) = extract_fixture("NetworkService.swift");
    let e = find_sym(&syms, "NetworkError").expect("NetworkError missing");
    assert_eq!(e.kind, SymbolKind::Enum);
    assert!(find_sym(&syms, "NetworkError.badStatus").is_some(), "badStatus variant missing");
    assert!(find_sym(&syms, "NetworkError.invalidResponse").is_some(), "invalidResponse variant missing");
}

// ---------------------------------------------------------------------------
// Inline unit tests for new features (subscript, computed property, generics)
// Kept here to stay within swift.rs line budget.
// ---------------------------------------------------------------------------

fn extract_inline(src: &str) -> (Vec<ast_graph_core::SymbolNode>, Vec<ast_graph_core::RawEdge>) {
    let extractor = SwiftExtractor;
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&extractor.tree_sitter_language()).expect("load failed");
    let tree = parser.parse(src.as_bytes(), None).expect("parse failed");
    let r = extractor.extract(src.as_bytes(), &tree, Path::new("inline.swift"));
    (r.symbols, r.raw_edges)
}

fn find_inline<'a>(syms: &'a [ast_graph_core::SymbolNode], name: &str) -> Option<&'a ast_graph_core::SymbolNode> {
    syms.iter().find(|s| s.name == name)
}

// --- subscript_declaration ---

#[test]
fn extracts_subscript_as_method() {
    let src = "class Box {\n  subscript(i: Int) -> String {\n    return \"x\"\n  }\n}\n";
    let (syms, _) = extract_inline(src);
    let s = find_inline(&syms, "Box.__subscript").expect("Box.__subscript missing");
    assert_eq!(s.kind, SymbolKind::Method);
}

#[test]
fn subscript_signature_includes_params_and_return_type() {
    let src = "class Grid {\n  subscript(row: Int, col: Int) -> Int {\n    return 0\n  }\n}\n";
    let (syms, _) = extract_inline(src);
    let s = find_inline(&syms, "Grid.__subscript").expect("Grid.__subscript missing");
    let sig = s.signature.as_deref().unwrap_or("");
    assert!(sig.contains("subscript"), "sig was: {sig:?}");
    assert!(sig.contains("-> Int"), "sig was: {sig:?}");
}

// --- computed property ---

#[test]
fn computed_property_has_computed_signature_prefix() {
    let src = "class Person {\n  var fullName: String { return \"x\" }\n}\n";
    let (syms, _) = extract_inline(src);
    let p = find_inline(&syms, "Person.fullName").expect("Person.fullName missing");
    assert_eq!(p.kind, SymbolKind::Property);
    let sig = p.signature.as_deref().unwrap_or("");
    assert!(sig.starts_with("computed var"), "expected 'computed var' prefix, got: {sig:?}");
}

#[test]
fn stored_property_has_plain_var_prefix() {
    let src = "class Foo {\n  var count: Int = 0\n}\n";
    let (syms, _) = extract_inline(src);
    let p = find_inline(&syms, "Foo.count").expect("Foo.count missing");
    let sig = p.signature.as_deref().unwrap_or("");
    assert!(sig.starts_with("var"), "expected 'var' prefix, got: {sig:?}");
    assert!(!sig.starts_with("computed"), "should not be computed, got: {sig:?}");
}

#[test]
fn computed_property_with_getter_setter_emits_calls() {
    let src = "class Foo {\n  var x: Int {\n    get { return helper() }\n    set { storeValue(newValue) }\n  }\n  func helper() -> Int { 0 }\n  func storeValue(_ v: Int) {}\n}\n";
    let (syms, edges) = extract_inline(src);
    let prop = find_inline(&syms, "Foo.x").expect("Foo.x missing");
    let calls: Vec<&str> = edges.iter()
        .filter(|e| e.source == prop.id && e.kind == EdgeKind::Calls)
        .map(|e| e.target_name.as_str())
        .collect();
    assert!(calls.iter().any(|t| t.contains("helper")), "expected helper call, got: {calls:?}");
    assert!(calls.iter().any(|t| t.contains("storeValue")), "expected storeValue call, got: {calls:?}");
}

#[test]
fn computed_property_shorthand_getter_emits_calls() {
    let src = "class Foo {\n  var doubled: Int { getValue() * 2 }\n  func getValue() -> Int { 0 }\n}\n";
    let (syms, edges) = extract_inline(src);
    let prop = find_inline(&syms, "Foo.doubled").expect("Foo.doubled missing");
    let calls: Vec<&str> = edges.iter()
        .filter(|e| e.source == prop.id && e.kind == EdgeKind::Calls)
        .map(|e| e.target_name.as_str())
        .collect();
    assert!(calls.iter().any(|t| t.contains("getValue")), "expected getValue call, got: {calls:?}");
}

// --- generic type parameter tracking ---

#[test]
fn generic_func_variable_receiver_call_preserved() {
    // Receiver "t" is a variable, not a type param name — falls through to plain text.
    let src = "func wrap<T>(_ t: T) -> T { t.transform() }\n";
    let (syms, edges) = extract_inline(src);
    let f = find_inline(&syms, "wrap").expect("wrap missing");
    let calls: Vec<&str> = edges.iter()
        .filter(|e| e.source == f.id && e.kind == EdgeKind::Calls)
        .map(|e| e.target_name.as_str())
        .collect();
    assert!(calls.iter().any(|c| c.contains("transform")), "expected transform call, got: {calls:?}");
}

#[test]
fn generic_func_type_param_receiver_gets_annotation() {
    // Receiver "T" exactly matches type param "T" → annotated as "T<T>.describe".
    let src = "func inspect<T>(_ t: T) { T.describe() }\n";
    let (syms, edges) = extract_inline(src);
    let f = find_inline(&syms, "inspect").expect("inspect missing");
    let calls: Vec<&str> = edges.iter()
        .filter(|e| e.source == f.id && e.kind == EdgeKind::Calls)
        .map(|e| e.target_name.as_str())
        .collect();
    assert!(calls.iter().any(|c| c.contains("T") && c.contains("describe")),
        "expected annotated T.describe call, got: {calls:?}");
}
