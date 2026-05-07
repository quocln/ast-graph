/// Integration tests loading real-world-style Ruby fixture files.
/// Validates that RubyExtractor + Rails layer correctly handles:
///   - ActiveRecord model with associations, callbacks, scopes, enums, ivars
///   - Rails controller with before_action, private methods, CRUD actions
///   - config/routes.rb with resources, nested routes, namespaces
use ast_graph_core::{EdgeKind, SymbolKind, Visibility};
use ast_graph_parse::extractor::LanguageExtractor;
use ast_graph_parse::lang::ruby::RubyExtractor;
use std::path::Path;

fn extract_fixture(rel_path: &str) -> (Vec<ast_graph_core::SymbolNode>, Vec<ast_graph_core::RawEdge>) {
    let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/ruby")
        .join(rel_path);
    let source = std::fs::read(&fixture_path)
        .unwrap_or_else(|e| panic!("failed to read fixture {rel_path}: {e}"));
    let extractor = RubyExtractor;
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&extractor.tree_sitter_language())
        .expect("tree-sitter-ruby load failed");
    let tree = parser
        .parse(&source, None)
        .expect("parse failed");
    let result = extractor.extract(&source, &tree, &fixture_path);
    (result.symbols, result.raw_edges)
}

fn find_sym<'a>(syms: &'a [ast_graph_core::SymbolNode], name: &str) -> Option<&'a ast_graph_core::SymbolNode> {
    syms.iter().find(|s| s.name == name)
}

fn refs_from<'a>(edges: &'a [ast_graph_core::RawEdge], src: ast_graph_core::NodeId) -> Vec<&'a str> {
    edges.iter()
        .filter(|e| e.source == src && e.kind == EdgeKind::References)
        .map(|e| e.target_name.as_str())
        .collect()
}

fn calls_from<'a>(edges: &'a [ast_graph_core::RawEdge], src: ast_graph_core::NodeId) -> Vec<&'a str> {
    edges.iter()
        .filter(|e| e.source == src && e.kind == EdgeKind::Calls)
        .map(|e| e.target_name.as_str())
        .collect()
}

fn impls_from<'a>(edges: &'a [ast_graph_core::RawEdge], src: ast_graph_core::NodeId) -> Vec<&'a str> {
    edges.iter()
        .filter(|e| e.source == src && e.kind == EdgeKind::Implements)
        .map(|e| e.target_name.as_str())
        .collect()
}

// ---------------------------------------------------------------------------
// user_model.rb — ActiveRecord model
// ---------------------------------------------------------------------------

#[test]
fn user_model_class_extracted() {
    let (syms, _) = extract_fixture("user_model.rb");
    let cls = find_sym(&syms, "User").expect("User class missing");
    assert_eq!(cls.kind, SymbolKind::Class);
    let sig = cls.signature.as_deref().unwrap_or("");
    assert!(sig.contains("ApplicationRecord"), "expected superclass, got: {sig:?}");
}

#[test]
fn user_model_extends_application_record() {
    let (syms, edges) = extract_fixture("user_model.rb");
    let user = find_sym(&syms, "User").unwrap();
    assert!(
        edges.iter().any(|e| e.source == user.id && e.kind == EdgeKind::Extends && e.target_name == "ApplicationRecord"),
        "expected EXTENDS ApplicationRecord"
    );
}

#[test]
fn user_model_includes_mixins() {
    let (syms, edges) = extract_fixture("user_model.rb");
    let user = find_sym(&syms, "User").unwrap();
    let impls = impls_from(&edges, user.id);
    assert!(impls.contains(&"Searchable"), "expected Searchable mixin, got: {impls:?}");
    assert!(impls.contains(&"Auditable"), "expected Auditable mixin, got: {impls:?}");
}

#[test]
fn user_model_associations_extracted() {
    let (syms, edges) = extract_fixture("user_model.rb");
    // belongs_to :organization → Property + REFERENCES Organization
    let org = find_sym(&syms, "User.organization").expect("User.organization missing");
    assert_eq!(org.kind, SymbolKind::Property);
    assert!(refs_from(&edges, org.id).contains(&"Organization"), "expected REFERENCES Organization");

    // has_many :posts → Property + REFERENCES Post
    let posts = find_sym(&syms, "User.posts").expect("User.posts missing");
    assert!(refs_from(&edges, posts.id).contains(&"Post"), "expected REFERENCES Post");

    // has_many :comments, class_name: 'Comment' → REFERENCES Comment
    let comments = find_sym(&syms, "User.comments").expect("User.comments missing");
    assert!(refs_from(&edges, comments.id).contains(&"Comment"), "expected REFERENCES Comment");

    // has_one :profile → REFERENCES Profile
    let profile = find_sym(&syms, "User.profile").expect("User.profile missing");
    assert!(refs_from(&edges, profile.id).contains(&"Profile"), "expected REFERENCES Profile");
}

#[test]
fn user_model_attr_accessor_properties() {
    let (syms, _) = extract_fixture("user_model.rb");
    assert_eq!(find_sym(&syms, "User.temporary_password").unwrap().kind, SymbolKind::Property);
    assert_eq!(find_sym(&syms, "User.skip_normalization").unwrap().kind, SymbolKind::Property);
}

#[test]
fn user_model_scopes_extracted_as_methods() {
    let (syms, _) = extract_fixture("user_model.rb");
    assert_eq!(find_sym(&syms, "User.active").unwrap().kind, SymbolKind::Method);
    assert_eq!(find_sym(&syms, "User.admins").unwrap().kind, SymbolKind::Method);
}

#[test]
fn user_model_callbacks_emit_calls_edges() {
    let (syms, edges) = extract_fixture("user_model.rb");
    let user = find_sym(&syms, "User").unwrap();
    let calls = calls_from(&edges, user.id);
    assert!(calls.contains(&"User.normalize_email"), "expected normalize_email callback edge");
    assert!(calls.contains(&"User.send_welcome_email"), "expected send_welcome_email callback edge");
    assert!(calls.contains(&"User.create_default_profile"), "expected create_default_profile callback edge");
}

#[test]
fn user_model_enum_predicate_methods_emitted() {
    let (syms, _) = extract_fixture("user_model.rb");
    assert!(find_sym(&syms, "User.member?").is_some(), "User.member? missing");
    assert!(find_sym(&syms, "User.member!").is_some(), "User.member! missing");
    assert!(find_sym(&syms, "User.admin?").is_some(), "User.admin? missing");
    assert!(find_sym(&syms, "User.admin!").is_some(), "User.admin! missing");
}

#[test]
fn user_model_instance_methods_with_visibility() {
    let (syms, _) = extract_fixture("user_model.rb");
    // Public methods
    assert_eq!(find_sym(&syms, "User.full_name").unwrap().visibility, Visibility::Public);
    assert_eq!(find_sym(&syms, "User.display_name").unwrap().visibility, Visibility::Public);
    assert_eq!(find_sym(&syms, "User.activate!").unwrap().visibility, Visibility::Public);
    // Private methods (after `private` keyword)
    assert_eq!(find_sym(&syms, "User.normalize_email").unwrap().visibility, Visibility::Private);
    assert_eq!(find_sym(&syms, "User.send_welcome_email").unwrap().visibility, Visibility::Private);
}

#[test]
fn user_model_ivar_properties_extracted() {
    let (syms, _) = extract_fixture("user_model.rb");
    // @display_cache assigned in display_name method
    let dp = find_sym(&syms, "User.@display_cache");
    assert!(dp.is_some(), "User.@display_cache missing (assigned in initialize)");
    assert_eq!(dp.unwrap().kind, SymbolKind::Property);
}

#[test]
fn user_model_symbol_count_reasonable() {
    let (syms, _) = extract_fixture("user_model.rb");
    // Sanity: more than 20 symbols expected (class + associations + methods + ivars + enum predicates)
    let non_file: Vec<_> = syms.iter().filter(|s| s.kind != SymbolKind::File).collect();
    assert!(non_file.len() >= 20, "expected ≥20 symbols, got {}", non_file.len());
}

// ---------------------------------------------------------------------------
// posts_controller.rb — Rails controller
// ---------------------------------------------------------------------------

#[test]
fn posts_controller_class_extracted() {
    let (syms, _) = extract_fixture("posts_controller.rb");
    let cls = find_sym(&syms, "PostsController").expect("PostsController missing");
    assert_eq!(cls.kind, SymbolKind::Class);
}

#[test]
fn posts_controller_extends_application_controller() {
    let (syms, edges) = extract_fixture("posts_controller.rb");
    let ctrl = find_sym(&syms, "PostsController").unwrap();
    assert!(
        edges.iter().any(|e| e.source == ctrl.id && e.kind == EdgeKind::Extends && e.target_name == "ApplicationController"),
        "expected EXTENDS ApplicationController"
    );
}

#[test]
fn posts_controller_before_action_callbacks() {
    let (syms, edges) = extract_fixture("posts_controller.rb");
    let ctrl = find_sym(&syms, "PostsController").unwrap();
    let calls = calls_from(&edges, ctrl.id);
    assert!(calls.contains(&"PostsController.authenticate_user!"), "expected authenticate_user! callback");
    assert!(calls.contains(&"PostsController.find_post"), "expected find_post callback");
    assert!(calls.contains(&"PostsController.authorize_post!"), "expected authorize_post! callback");
}

#[test]
fn posts_controller_public_actions_extracted() {
    let (syms, _) = extract_fixture("posts_controller.rb");
    for action in &["index", "show", "new", "create", "edit", "update", "destroy"] {
        let name = format!("PostsController.{action}");
        assert!(find_sym(&syms, &name).is_some(), "{name} missing");
        assert_eq!(find_sym(&syms, &name).unwrap().visibility, Visibility::Public, "{name} should be Public");
    }
}

#[test]
fn posts_controller_private_methods_have_private_visibility() {
    let (syms, _) = extract_fixture("posts_controller.rb");
    for method in &["find_post", "authorize_post!", "current_user_can_edit?", "post_params"] {
        let name = format!("PostsController.{method}");
        assert_eq!(
            find_sym(&syms, &name).unwrap_or_else(|| panic!("{name} missing")).visibility,
            Visibility::Private,
            "{name} should be Private"
        );
    }
}

#[test]
fn posts_controller_ivar_in_action_extracted() {
    let (syms, _) = extract_fixture("posts_controller.rb");
    // @posts assigned in index action
    assert!(find_sym(&syms, "PostsController.@posts").is_some(), "PostsController.@posts missing");
    // @post assigned in show action (or find_post)
    assert!(find_sym(&syms, "PostsController.@post").is_some(), "PostsController.@post missing");
}

// ---------------------------------------------------------------------------
// routes.rb — Rails config/routes.rb
// ---------------------------------------------------------------------------

#[test]
fn routes_module_extracted() {
    let (syms, _) = extract_fixture("routes.rb");
    // Routes extractor emits a synthetic "Routes" module symbol
    let routes = find_sym(&syms, "Routes");
    assert!(routes.is_some(), "Routes module missing");
    assert_eq!(routes.unwrap().kind, SymbolKind::Module);
}

#[test]
fn routes_users_controller_actions_called() {
    let (syms, edges) = extract_fixture("routes.rb");
    let routes = find_sym(&syms, "Routes").expect("Routes missing");
    let calls = calls_from(&edges, routes.id);
    // `resources :users` → 7 standard CRUD actions
    assert!(calls.iter().any(|c| c.contains("UsersController") && c.contains("index")),
        "expected UsersController.index in calls, got: {calls:?}");
    assert!(calls.iter().any(|c| c.contains("UsersController") && c.contains("show")),
        "expected UsersController.show in calls");
    assert!(calls.iter().any(|c| c.contains("UsersController") && c.contains("create")),
        "expected UsersController.create in calls");
    assert!(calls.iter().any(|c| c.contains("UsersController") && c.contains("destroy")),
        "expected UsersController.destroy in calls");
}

#[test]
fn routes_posts_controller_actions_called() {
    let (syms, edges) = extract_fixture("routes.rb");
    let routes = find_sym(&syms, "Routes").expect("Routes missing");
    let calls = calls_from(&edges, routes.id);
    assert!(calls.iter().any(|c| c.contains("PostsController") && c.contains("index")),
        "expected PostsController.index in calls");
    assert!(calls.iter().any(|c| c.contains("PostsController") && c.contains("publish")),
        "expected PostsController.publish (member action) in calls");
}

#[test]
fn routes_session_controller_verb_actions_called() {
    let (syms, edges) = extract_fixture("routes.rb");
    let routes = find_sym(&syms, "Routes").expect("Routes missing");
    let calls = calls_from(&edges, routes.id);
    // `get '/login', to: 'sessions#new'` → SessionsController.new
    assert!(
        calls.iter().any(|c| c.contains("SessionsController") && c.contains("new")),
        "expected SessionsController.new from direct HTTP verb route, got: {calls:?}"
    );
}

#[test]
fn routes_total_call_edges_reasonable() {
    let (syms, edges) = extract_fixture("routes.rb");
    let routes = find_sym(&syms, "Routes").expect("Routes missing");
    let call_count = edges.iter().filter(|e| e.source == routes.id && e.kind == EdgeKind::Calls).count();
    // users(7) + users member(2) + users collection(1) + nested posts(2)
    // + posts(7) + posts member(2) + posts collection(1) + comments(2)
    // + admin users(7) + admin posts(5) + sessions(3) = 39+
    assert!(call_count >= 30, "expected ≥30 route call edges, got {call_count}");
}
