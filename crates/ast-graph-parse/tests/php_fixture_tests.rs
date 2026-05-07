/// Integration tests loading real-world-style PHP fixture files.
/// Validates that PhpExtractor correctly handles:
///   - Laravel-style controller with namespace, DI constructor, Eloquent calls
///   - Symfony-style event listener with interface, traits, attribute-style DI
///   - PHP 8.1 repository with readonly constructor, enum usage, first-class callables
use ast_graph_core::{EdgeKind, SymbolKind, Visibility};
use ast_graph_parse::extractor::LanguageExtractor;
use ast_graph_parse::lang::php::PhpExtractor;
use std::path::Path;

fn extract_fixture(rel_path: &str) -> (Vec<ast_graph_core::SymbolNode>, Vec<ast_graph_core::RawEdge>) {
    let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/php")
        .join(rel_path);
    let source = std::fs::read(&fixture_path)
        .unwrap_or_else(|e| panic!("failed to read fixture {rel_path}: {e}"));
    let extractor = PhpExtractor;
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&extractor.tree_sitter_language())
        .expect("tree-sitter-php load failed");
    let tree = parser.parse(&source, None).expect("parse failed");
    let result = extractor.extract(&source, &tree, &fixture_path);
    (result.symbols, result.raw_edges)
}

fn find_sym<'a>(
    syms: &'a [ast_graph_core::SymbolNode],
    name: &str,
) -> Option<&'a ast_graph_core::SymbolNode> {
    syms.iter().find(|s| s.name == name)
}

fn calls_from<'a>(
    edges: &'a [ast_graph_core::RawEdge],
    src: ast_graph_core::NodeId,
) -> Vec<&'a str> {
    edges
        .iter()
        .filter(|e| e.source == src && e.kind == EdgeKind::Calls)
        .map(|e| e.target_name.as_str())
        .collect()
}

fn impls_from<'a>(
    edges: &'a [ast_graph_core::RawEdge],
    src: ast_graph_core::NodeId,
) -> Vec<&'a str> {
    edges
        .iter()
        .filter(|e| e.source == src && e.kind == EdgeKind::Implements)
        .map(|e| e.target_name.as_str())
        .collect()
}

fn refs_from<'a>(
    edges: &'a [ast_graph_core::RawEdge],
    src: ast_graph_core::NodeId,
) -> Vec<&'a str> {
    edges
        .iter()
        .filter(|e| e.source == src && e.kind == EdgeKind::References)
        .map(|e| e.target_name.as_str())
        .collect()
}

// ---------------------------------------------------------------------------
// user_controller.php — Laravel-style controller
// ---------------------------------------------------------------------------

#[test]
fn user_controller_class_extracted() {
    let (syms, _) = extract_fixture("user_controller.php");
    let cls = find_sym(
        &syms,
        "App\\Http\\Controllers\\UserController",
    )
    .expect("UserController missing");
    assert_eq!(cls.kind, SymbolKind::Class);
}

#[test]
fn user_controller_extends_controller() {
    let (syms, edges) = extract_fixture("user_controller.php");
    let ctrl = find_sym(&syms, "App\\Http\\Controllers\\UserController").unwrap();
    assert!(
        edges.iter().any(|e| {
            e.source == ctrl.id && e.kind == EdgeKind::Extends && e.target_name == "Controller"
        }),
        "expected EXTENDS Controller"
    );
}

#[test]
fn user_controller_constructor_is_constructor_kind() {
    let (syms, _) = extract_fixture("user_controller.php");
    let ctor = find_sym(
        &syms,
        "App\\Http\\Controllers\\UserController.__construct",
    )
    .expect("__construct missing");
    assert_eq!(ctor.kind, SymbolKind::Constructor);
}

#[test]
fn user_controller_public_actions_extracted() {
    let (syms, _) = extract_fixture("user_controller.php");
    let prefix = "App\\Http\\Controllers\\UserController";
    for action in &["index", "show", "store", "update", "destroy"] {
        let name = format!("{prefix}.{action}");
        let m = find_sym(&syms, &name).unwrap_or_else(|| panic!("{name} missing"));
        assert_eq!(m.kind, SymbolKind::Method);
        assert_eq!(m.visibility, Visibility::Public, "{name} should be Public");
    }
}

#[test]
fn user_controller_index_calls_user_service_paginate() {
    let (syms, edges) = extract_fixture("user_controller.php");
    let index = find_sym(
        &syms,
        "App\\Http\\Controllers\\UserController.index",
    )
    .expect("index missing");
    let calls = calls_from(&edges, index.id);
    // $this->userService->paginate(...) → target contains "paginate"
    assert!(
        calls.iter().any(|c| c.contains("paginate")),
        "expected paginate call, got: {calls:?}"
    );
}

#[test]
fn user_controller_store_calls_user_service_create() {
    let (syms, edges) = extract_fixture("user_controller.php");
    let store = find_sym(
        &syms,
        "App\\Http\\Controllers\\UserController.store",
    )
    .expect("store missing");
    let calls = calls_from(&edges, store.id);
    assert!(
        calls.iter().any(|c| c.contains("create")),
        "expected create call, got: {calls:?}"
    );
}

#[test]
fn user_controller_use_imports_extracted() {
    let (_, edges) = extract_fixture("user_controller.php");
    let imports: Vec<_> = edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Imports)
        .map(|e| e.target_name.as_str())
        .collect();
    assert!(imports.contains(&"App\\Models\\User"), "missing User import");
    assert!(
        imports.contains(&"App\\Services\\UserService"),
        "missing UserService import"
    );
    assert!(
        imports.contains(&"Illuminate\\Http\\Request"),
        "missing Request import"
    );
}

#[test]
fn user_controller_symbol_count_reasonable() {
    let (syms, _) = extract_fixture("user_controller.php");
    let non_file: Vec<_> = syms.iter().filter(|s| s.kind != SymbolKind::File).collect();
    // namespace + imports + class + 6 methods ≥ 10
    assert!(
        non_file.len() >= 10,
        "expected ≥10 symbols, got {}",
        non_file.len()
    );
}

// ---------------------------------------------------------------------------
// event_listener.php — Symfony-style event listener
// ---------------------------------------------------------------------------

#[test]
fn event_listener_class_extracted() {
    let (syms, _) = extract_fixture("event_listener.php");
    let cls =
        find_sym(&syms, "App\\EventListeners\\UserRegisteredListener").expect("listener missing");
    assert_eq!(cls.kind, SymbolKind::Class);
}

#[test]
fn event_listener_implements_interface() {
    let (syms, edges) = extract_fixture("event_listener.php");
    let cls = find_sym(&syms, "App\\EventListeners\\UserRegisteredListener").unwrap();
    let impls = impls_from(&edges, cls.id);
    assert!(
        impls.contains(&"EventListenerInterface"),
        "expected EventListenerInterface, got: {impls:?}"
    );
}

#[test]
fn event_listener_uses_traits() {
    let (syms, edges) = extract_fixture("event_listener.php");
    let cls = find_sym(&syms, "App\\EventListeners\\UserRegisteredListener").unwrap();
    let impls = impls_from(&edges, cls.id);
    assert!(
        impls.contains(&"LogsActivity"),
        "expected LogsActivity trait, got: {impls:?}"
    );
    assert!(
        impls.contains(&"SendsNotifications"),
        "expected SendsNotifications trait, got: {impls:?}"
    );
}

#[test]
fn event_listener_handle_method_extracted() {
    let (syms, _) = extract_fixture("event_listener.php");
    let method = find_sym(
        &syms,
        "App\\EventListeners\\UserRegisteredListener.handle",
    )
    .expect("handle method missing");
    assert_eq!(method.kind, SymbolKind::Method);
    assert_eq!(method.visibility, Visibility::Public);
}

#[test]
fn event_listener_handle_calls_mailer_and_audit() {
    let (syms, edges) = extract_fixture("event_listener.php");
    let handle = find_sym(
        &syms,
        "App\\EventListeners\\UserRegisteredListener.handle",
    )
    .expect("handle missing");
    let calls = calls_from(&edges, handle.id);
    assert!(
        calls.iter().any(|c| c.contains("sendWelcome")),
        "expected sendWelcome call, got: {calls:?}"
    );
    assert!(
        calls.iter().any(|c| c.contains("record")),
        "expected audit.record call, got: {calls:?}"
    );
}

// ---------------------------------------------------------------------------
// repository.php — PHP 8.1+ repository with enum and first-class callable
// ---------------------------------------------------------------------------

#[test]
fn repository_class_extracted() {
    let (syms, _) = extract_fixture("repository.php");
    let cls = find_sym(&syms, "App\\Repositories\\UserRepository").expect("UserRepository missing");
    assert_eq!(cls.kind, SymbolKind::Class);
}

#[test]
fn repository_implements_interface() {
    let (syms, edges) = extract_fixture("repository.php");
    let cls = find_sym(&syms, "App\\Repositories\\UserRepository").unwrap();
    let impls = impls_from(&edges, cls.id);
    assert!(
        impls.contains(&"UserRepositoryInterface"),
        "expected UserRepositoryInterface, got: {impls:?}"
    );
}

#[test]
fn repository_public_methods_extracted() {
    let (syms, _) = extract_fixture("repository.php");
    let prefix = "App\\Repositories\\UserRepository";
    for method in &["find", "findAllActive", "save", "delete"] {
        let name = format!("{prefix}.{method}");
        assert!(find_sym(&syms, &name).is_some(), "{name} missing");
    }
}

#[test]
fn repository_hydrate_is_private() {
    let (syms, _) = extract_fixture("repository.php");
    let m = find_sym(&syms, "App\\Repositories\\UserRepository.hydrate").expect("hydrate missing");
    assert_eq!(m.visibility, Visibility::Private);
}

#[test]
fn repository_find_active_emits_references_to_hydrate() {
    // `array_map($this->hydrate(...), $rows)` — first-class callable reference.
    let (syms, edges) = extract_fixture("repository.php");
    let method =
        find_sym(&syms, "App\\Repositories\\UserRepository.findAllActive").expect("findAllActive missing");
    let refs = refs_from(&edges, method.id);
    assert!(
        refs.iter().any(|r| r.contains("hydrate")),
        "expected REFERENCES edge to hydrate (first-class callable), got: {refs:?}"
    );
}

#[test]
fn repository_doc_comment_on_class() {
    let (syms, _) = extract_fixture("repository.php");
    let cls = find_sym(&syms, "App\\Repositories\\UserRepository").unwrap();
    let doc = cls.doc_comment.as_deref().unwrap_or("");
    assert!(
        doc.contains("repository") || doc.contains("Repository"),
        "expected doc comment, got: {doc:?}"
    );
}
