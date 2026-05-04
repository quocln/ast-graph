//! Rails routes.rb parser.
//!
//! Recognizes `Rails.application.routes.draw do ... end` blocks and the
//! DSL inside (`resources`, `resource`, HTTP verbs, `namespace`, `scope`,
//! `member`, `collection`).  Emits a synthetic `Routes` Module symbol per
//! file, with CALLS edges from it to each controller action that the
//! routes declare.
//!
//! What the graph gains:
//!   * Controller actions appear as called from "Routes" → no longer
//!     flagged as dead code by `dead-code`.
//!   * Queries like "what routes hit UsersController?" become
//!     `MATCH (r:Symbol{name:"Routes"})-[:CALLS]->(a:Symbol)
//!         WHERE a.name STARTS WITH 'UsersController.'`.
//!
//! Naming: controllers are emitted as bare `UsersController` (no module
//! prefix from `namespace :api`), matching how the Ruby extractor stores
//! `module Api; class UsersController` without qualifying.  Resolver does
//! name-only fallback for cross-file matches.

use ast_graph_core::*;
use crate::extractor::*;
use inflector::Inflector;
use std::path::Path;

/// Camelize without singularizing — Rails controllers keep their plural
/// form (`users` → `Users`, not `User`).  `Inflector::to_class_case` does
/// singular + camel together so it's the wrong tool for controller names.
fn camelize_plural(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for word in name.split('_').filter(|w| !w.is_empty()) {
        let mut chars = word.chars();
        if let Some(c) = chars.next() {
            for u in c.to_uppercase() {
                out.push(u);
            }
            out.push_str(chars.as_str());
        }
    }
    out
}

/// Standard 7 actions emitted by `resources :foo`.
const RESOURCES_ACTIONS: &[&str] = &[
    "index", "show", "new", "create", "edit", "update", "destroy",
];

/// Standard 6 actions emitted by `resource :foo` (singular — no index).
const RESOURCE_ACTIONS: &[&str] = &[
    "show", "new", "create", "edit", "update", "destroy",
];

const HTTP_VERBS: &[&str] = &["get", "post", "put", "patch", "delete", "match"];

/// Try to recognize `<...>routes.draw do ... end` at `call_node`.
/// Returns `true` if the call was a routes block (so the caller knows
/// not to treat it as a regular call).
pub fn try_recognize_routes_draw(
    source: &[u8],
    call_node: &tree_sitter::Node,
    file_path: &Path,
    file_node_id: NodeId,
    symbols: &mut Vec<SymbolNode>,
    raw_edges: &mut Vec<RawEdge>,
) -> bool {
    if call_node.kind() != "call" {
        return false;
    }
    let method_node = match child_by_field(call_node, "method") {
        Some(n) => n,
        None => return false,
    };
    if node_text(source, &method_node) != "draw" {
        return false;
    }
    // Receiver chain must end in `routes` — guards against random `.draw`
    // calls in non-routes code (rare but possible).
    let receiver_text = match child_by_field(call_node, "receiver") {
        Some(r) => node_text(source, &r),
        None => return false,
    };
    if !receiver_text.ends_with("routes") && !receiver_text.ends_with("routes()") {
        return false;
    }
    let block = match child_by_field(call_node, "block") {
        Some(b) => b,
        None => return false,
    };
    let body = match child_by_field(&block, "body") {
        Some(b) => b,
        None => return false,
    };

    // Synthetic Routes container — sourced at the routes.draw line so the
    // line range is meaningful.
    let line = call_node.start_position().row as u32;
    let routes_id = NodeId::new(
        &file_path.to_string_lossy(),
        "Routes",
        SymbolKind::Module,
        line,
    );
    symbols.push(SymbolNode {
        id: routes_id,
        name: "Routes".to_string(),
        kind: SymbolKind::Module,
        file_path: file_path.to_path_buf(),
        line_range: (line, call_node.end_position().row as u32),
        signature: Some("Rails.application.routes.draw".to_string()),
        doc_comment: None,
        visibility: Visibility::Public,
        language: Language::Ruby,
        parent: Some(file_node_id),
    });

    // Walk the do_block body collecting routes.
    walk_routes_body(source, &body, file_path, routes_id, None, symbols, raw_edges);
    true
}

/// Walk a routes-DSL body. `current_resource` carries the controller name
/// when we're inside a `resources :foo do member do ... end end` block —
/// inside `member`/`collection`, bareword `get :extra` adds an action to
/// the enclosing resource.
fn walk_routes_body(
    source: &[u8],
    body: &tree_sitter::Node,
    file_path: &Path,
    routes_id: NodeId,
    current_resource: Option<&str>,
    symbols: &mut Vec<SymbolNode>,
    raw_edges: &mut Vec<RawEdge>,
) {
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        if child.kind() != "call" {
            continue;
        }
        if child_by_field(&child, "receiver").is_some() {
            continue;
        }
        let method_node = match child_by_field(&child, "method") {
            Some(n) => n,
            None => continue,
        };
        let method = node_text(source, &method_node);
        let line = child.start_position().row as u32;
        match method {
            "resources" => handle_resources(
                source, &child, file_path, routes_id, /*plural=*/ true,
                symbols, raw_edges,
            ),
            "resource" => handle_resources(
                source, &child, file_path, routes_id, /*plural=*/ false,
                symbols, raw_edges,
            ),
            "namespace" | "scope" => {
                // Recurse into the nested block.  We don't track namespace
                // module prefixing — controllers are named bare and the
                // resolver matches by name.
                if let Some(b) = block_body(&child) {
                    walk_routes_body(source, &b, file_path, routes_id, current_resource, symbols, raw_edges);
                }
            }
            "member" | "collection" => {
                // Inside `resources :users do member do ... end end` — these
                // add custom actions to the enclosing resource.
                if let (Some(b), Some(res)) = (block_body(&child), current_resource) {
                    walk_member_collection_body(source, &b, res, routes_id, line, raw_edges);
                }
            }
            "root" => handle_root(source, &child, routes_id, line, raw_edges),
            v if HTTP_VERBS.contains(&v) => {
                handle_verb(source, &child, routes_id, line, raw_edges);
            }
            _ => {}
        }
    }
}

/// `resources :users [do ... end]` / `resource :profile [do ... end]`
fn handle_resources(
    source: &[u8],
    call: &tree_sitter::Node,
    file_path: &Path,
    routes_id: NodeId,
    plural: bool,
    symbols: &mut Vec<SymbolNode>,
    raw_edges: &mut Vec<RawEdge>,
) {
    // Args: first symbol is the resource name; later args may include
    // `only: [:show, :index]` or `except: [:destroy]` to narrow actions,
    // and `controller: 'OtherController'` to override the controller.
    let args_node = match child_by_field(call, "arguments") {
        Some(a) => a,
        None => return,
    };

    let mut name: Option<String> = None;
    let mut only: Option<Vec<String>> = None;
    let mut except: Option<Vec<String>> = None;
    let mut controller_override: Option<String> = None;

    let mut cursor = args_node.walk();
    for arg in args_node.children(&mut cursor) {
        if !arg.is_named() {
            continue;
        }
        match arg.kind() {
            "simple_symbol" if name.is_none() => {
                name = Some(node_text(source, &arg).trim_start_matches(':').to_string());
            }
            "pair" => {
                let mut c = arg.walk();
                let nodes: Vec<_> = arg.children(&mut c).filter(|n| n.is_named()).collect();
                if nodes.len() < 2 {
                    continue;
                }
                let key = node_text(source, &nodes[0])
                    .trim_end_matches(':').trim_start_matches(':').to_string();
                let value_node = &nodes[1];
                match key.as_str() {
                    "only" => only = Some(collect_action_names(source, value_node)),
                    "except" => except = Some(collect_action_names(source, value_node)),
                    "controller" => {
                        // Value is a string literal — keep as-is, camelize + Controller.
                        let raw = node_text(source, value_node);
                        let bare = raw.trim_matches(|c| c == '"' || c == '\'').to_string();
                        controller_override = Some(if bare.ends_with("Controller") {
                            bare
                        } else {
                            format!("{}Controller", camelize_plural(&bare))
                        });
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let Some(name) = name else { return };
    let controller = controller_override.unwrap_or_else(|| {
        // `users` → `UsersController`, `user` → `UsersController` (singular `resource`
        // routes still hit a plural-named controller).
        let plural_name = if plural { name.clone() } else { name.to_plural() };
        format!("{}Controller", camelize_plural(&plural_name))
    });

    let actions = if plural { RESOURCES_ACTIONS } else { RESOURCE_ACTIONS };
    let line = call.start_position().row as u32;

    for action in actions {
        if let Some(o) = &only {
            if !o.iter().any(|a| a == action) {
                continue;
            }
        }
        if let Some(e) = &except {
            if e.iter().any(|a| a == action) {
                continue;
            }
        }
        emit_route_call(routes_id, &controller, action, line, raw_edges);
    }

    // Recurse into nested do_block to handle `member`/`collection`/nested resources.
    if let Some(body) = block_body(call) {
        walk_routes_body(source, &body, file_path, routes_id, Some(&controller), symbols, raw_edges);
    }
}

/// Walk a `member do ... end` or `collection do ... end` body.  Each call
/// to an HTTP verb here adds an action to the enclosing resource's
/// controller.
fn walk_member_collection_body(
    source: &[u8],
    body: &tree_sitter::Node,
    controller: &str,
    routes_id: NodeId,
    _line: u32,
    raw_edges: &mut Vec<RawEdge>,
) {
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        if child.kind() != "call" {
            continue;
        }
        if child_by_field(&child, "receiver").is_some() {
            continue;
        }
        let method_node = match child_by_field(&child, "method") {
            Some(n) => n,
            None => continue,
        };
        let method = node_text(source, &method_node);
        if !HTTP_VERBS.contains(&method) {
            continue;
        }
        // Inside member/collection, `get :extra` means "action :extra on
        // the enclosing controller".  Walk args looking for a symbol
        // (action name) or a string with `controller#action` form.
        let line = child.start_position().row as u32;
        if let Some(args) = child_by_field(&child, "arguments") {
            let mut c2 = args.walk();
            for arg in args.children(&mut c2) {
                if !arg.is_named() {
                    continue;
                }
                if arg.kind() == "simple_symbol" {
                    let action = node_text(source, &arg).trim_start_matches(':').to_string();
                    emit_route_call(routes_id, controller, &action, line, raw_edges);
                    break;
                }
                // String shape `'/extra' => 'controller#action'` parses as a
                // pair; let handle_verb deal with it for full generality.
            }
        }
        // Also try the full handle_verb pathway for `controller#action` style.
        handle_verb(source, &child, routes_id, line, raw_edges);
    }
}

/// HTTP verb at the top level of routes.draw — outside a `resources` block.
/// Forms:
///   * `get '/path' => 'controller#action'`
///   * `get '/path', to: 'controller#action'`
///   * `get '/path', controller: 'foo', action: 'bar'`
fn handle_verb(
    source: &[u8],
    call: &tree_sitter::Node,
    routes_id: NodeId,
    line: u32,
    raw_edges: &mut Vec<RawEdge>,
) {
    let args_node = match child_by_field(call, "arguments") {
        Some(a) => a,
        None => return,
    };
    let mut cursor = args_node.walk();
    let mut explicit_controller: Option<String> = None;
    let mut explicit_action: Option<String> = None;
    let mut to_value: Option<String> = None;
    let mut arrow_target: Option<String> = None;

    for arg in args_node.children(&mut cursor) {
        if !arg.is_named() {
            continue;
        }
        match arg.kind() {
            "pair" => {
                let mut c = arg.walk();
                let nodes: Vec<_> = arg.children(&mut c).filter(|n| n.is_named()).collect();
                if nodes.len() < 2 {
                    continue;
                }
                let key_text = node_text(source, &nodes[0]);
                let key = key_text.trim_end_matches(':').trim_start_matches(':');
                let value_node = &nodes[1];
                let value_text = node_text(source, value_node);
                let value_str = value_text
                    .trim_matches(|c| c == '"' || c == '\'')
                    .to_string();
                match key {
                    "to" => to_value = Some(value_str),
                    "controller" => explicit_controller = Some(value_str),
                    "action" => explicit_action = Some(value_str),
                    _ => {
                        // Arrow-form: a string-keyed pair like `'/path' => 'c#a'`.
                        // Distinguish from kwarg pairs by checking the key node kind:
                        // arrow pairs have a string key, kwarg pairs have a hash_key_symbol.
                        if nodes[0].kind() == "string" {
                            arrow_target = Some(value_str);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Resolve controller#action from whichever form we matched.
    let target = arrow_target.or(to_value);
    if let Some(t) = target {
        if let Some((ctrl, act)) = parse_controller_action(&t) {
            emit_route_call(routes_id, &ctrl, &act, line, raw_edges);
            return;
        }
    }
    if let (Some(c), Some(a)) = (explicit_controller, explicit_action) {
        let ctrl = if c.ends_with("Controller") {
            c
        } else {
            format!("{}Controller", camelize_plural(&c))
        };
        emit_route_call(routes_id, &ctrl, &a, line, raw_edges);
    }
}

/// `root 'home#index'` / `root to: 'home#index'`
fn handle_root(
    source: &[u8],
    call: &tree_sitter::Node,
    routes_id: NodeId,
    line: u32,
    raw_edges: &mut Vec<RawEdge>,
) {
    let args_node = match child_by_field(call, "arguments") {
        Some(a) => a,
        None => return,
    };
    let mut cursor = args_node.walk();
    for arg in args_node.children(&mut cursor) {
        if !arg.is_named() {
            continue;
        }
        // String literal form: `root 'home#index'`.  Only attempt direct
        // controller#action parse on string args — for pair args, fall
        // through to the pair branch (otherwise the entire pair text
        // `to: 'home#index'` would be mis-parsed as a controller name).
        if arg.kind() == "string" {
            let raw = node_text(source, &arg);
            let stripped = raw.trim_matches(|c| c == '"' || c == '\'');
            if let Some((ctrl, act)) = parse_controller_action(stripped) {
                emit_route_call(routes_id, &ctrl, &act, line, raw_edges);
                return;
            }
        }
        // `root to: 'home#index'` — fall through pair handling
        if arg.kind() == "pair" {
            let mut c = arg.walk();
            let nodes: Vec<_> = arg.children(&mut c).filter(|n| n.is_named()).collect();
            if nodes.len() == 2 {
                let value_text = node_text(source, &nodes[1]);
                let value_str = value_text.trim_matches(|c| c == '"' || c == '\'');
                if let Some((ctrl, act)) = parse_controller_action(value_str) {
                    emit_route_call(routes_id, &ctrl, &act, line, raw_edges);
                    return;
                }
            }
        }
    }
}

/// Parse a `"controller#action"` shorthand into (`ControllerClass`, action).
/// `'users#index'` → (`UsersController`, `index`).  Returns `None` if the
/// string has no `#` separator or is malformed.
fn parse_controller_action(s: &str) -> Option<(String, String)> {
    let (ctrl, act) = s.split_once('#')?;
    if ctrl.is_empty() || act.is_empty() {
        return None;
    }
    let class_name = if ctrl.ends_with("Controller") {
        ctrl.to_string()
    } else {
        // `admin/users` → `Admin::UsersController`. We collapse to bare
        // `UsersController` to match how nested controllers are usually
        // stored (see naming note at top).
        let last = ctrl.rsplit('/').next().unwrap_or(ctrl);
        format!("{}Controller", camelize_plural(last))
    };
    Some((class_name, act.to_string()))
}

/// Collect symbol names from an argument value that's an array literal
/// (`[:show, :index]`) or a single symbol (`:show`).
fn collect_action_names(source: &[u8], node: &tree_sitter::Node) -> Vec<String> {
    let mut out = Vec::new();
    match node.kind() {
        "simple_symbol" => out.push(node_text(source, node).trim_start_matches(':').to_string()),
        "array" | "symbol_array" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if !child.is_named() {
                    continue;
                }
                match child.kind() {
                    "simple_symbol" | "bare_symbol" => {
                        out.push(node_text(source, &child).trim_start_matches(':').to_string());
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    out
}

/// Get the `do_block` / `block` body off a `call` node.  Bypasses the
/// `child_by_field` helper because that helper ties the returned node's
/// lifetime to the `&Node` reference, which a two-step chain through a
/// local can't satisfy.
fn block_body<'a>(call: &tree_sitter::Node<'a>) -> Option<tree_sitter::Node<'a>> {
    let block = call.child_by_field_name("block")?;
    block.child_by_field_name("body")
}

fn emit_route_call(
    routes_id: NodeId,
    controller: &str,
    action: &str,
    line: u32,
    raw_edges: &mut Vec<RawEdge>,
) {
    raw_edges.push(RawEdge {
        source: routes_id,
        kind: EdgeKind::Calls,
        target_name: format!("{controller}.{action}"),
        target_module: None,
        source_line: line,
    });
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
        let r = extractor.extract(src.as_bytes(), &tree, Path::new("config/routes.rb"));
        (r.symbols, r.raw_edges)
    }

    fn route_targets<'a>(edges: &'a [RawEdge], routes_id: NodeId) -> Vec<&'a str> {
        let mut v: Vec<&str> = edges.iter()
            .filter(|e| e.source == routes_id && e.kind == EdgeKind::Calls)
            .map(|e| e.target_name.as_str())
            .collect();
        v.sort_unstable();
        v
    }

    fn routes_id(syms: &[SymbolNode]) -> Option<NodeId> {
        syms.iter().find(|s| s.name == "Routes").map(|s| s.id)
    }

    #[test]
    fn resources_emits_seven_actions() {
        let src = "Rails.application.routes.draw do\n  resources :users\nend\n";
        let (syms, edges) = extract(src);
        let rid = routes_id(&syms).expect("Routes module missing");
        let targets = route_targets(&edges, rid);
        for action in RESOURCES_ACTIONS {
            let want = format!("UsersController.{action}");
            assert!(targets.iter().any(|t| *t == want), "missing {want}, got: {targets:?}");
        }
    }

    #[test]
    fn resource_singular_emits_six_actions_no_index() {
        let src = "Rails.application.routes.draw do\n  resource :profile\nend\n";
        let (syms, edges) = extract(src);
        let rid = routes_id(&syms).unwrap();
        let targets = route_targets(&edges, rid);
        // Singular `resource :profile` still uses ProfilesController (plural).
        assert!(targets.iter().any(|t| *t == "ProfilesController.show"));
        assert!(targets.iter().any(|t| *t == "ProfilesController.create"));
        assert!(!targets.iter().any(|t| *t == "ProfilesController.index"),
            "singular resource should NOT have index, got: {targets:?}");
    }

    #[test]
    fn resources_with_only_kwarg_narrows_actions() {
        let src = "Rails.application.routes.draw do\n  resources :users, only: [:index, :show]\nend\n";
        let (syms, edges) = extract(src);
        let rid = routes_id(&syms).unwrap();
        let targets = route_targets(&edges, rid);
        assert!(targets.contains(&"UsersController.index"));
        assert!(targets.contains(&"UsersController.show"));
        assert!(!targets.contains(&"UsersController.destroy"), "got: {targets:?}");
    }

    #[test]
    fn resources_with_except_kwarg_excludes_actions() {
        let src = "Rails.application.routes.draw do\n  resources :users, except: [:destroy]\nend\n";
        let (syms, edges) = extract(src);
        let rid = routes_id(&syms).unwrap();
        let targets = route_targets(&edges, rid);
        assert!(targets.contains(&"UsersController.index"));
        assert!(!targets.contains(&"UsersController.destroy"), "got: {targets:?}");
    }

    #[test]
    fn resources_with_controller_override() {
        let src = "Rails.application.routes.draw do\n  resources :users, controller: 'admins'\nend\n";
        let (syms, edges) = extract(src);
        let rid = routes_id(&syms).unwrap();
        let targets = route_targets(&edges, rid);
        assert!(targets.iter().any(|t| t == &"AdminsController.index"),
            "got: {targets:?}");
    }

    #[test]
    fn http_verb_arrow_form() {
        let src = "Rails.application.routes.draw do\n  get '/health' => 'system#health'\nend\n";
        let (syms, edges) = extract(src);
        let rid = routes_id(&syms).unwrap();
        let targets = route_targets(&edges, rid);
        assert!(targets.contains(&"SystemController.health"), "got: {targets:?}");
    }

    #[test]
    fn http_verb_to_kwarg_form() {
        let src = "Rails.application.routes.draw do\n  get '/health', to: 'system#health'\nend\n";
        let (syms, edges) = extract(src);
        let rid = routes_id(&syms).unwrap();
        let targets = route_targets(&edges, rid);
        assert!(targets.contains(&"SystemController.health"), "got: {targets:?}");
    }

    #[test]
    fn root_route() {
        let src = "Rails.application.routes.draw do\n  root 'home#index'\nend\n";
        let (syms, edges) = extract(src);
        let rid = routes_id(&syms).unwrap();
        let targets = route_targets(&edges, rid);
        assert!(targets.contains(&"HomeController.index"), "got: {targets:?}");
    }

    #[test]
    fn root_route_with_to_kwarg() {
        let src = "Rails.application.routes.draw do\n  root to: 'home#index'\nend\n";
        let (syms, edges) = extract(src);
        let rid = routes_id(&syms).unwrap();
        let targets = route_targets(&edges, rid);
        assert!(targets.contains(&"HomeController.index"));
    }

    #[test]
    fn namespace_block_recurses() {
        let src = r#"Rails.application.routes.draw do
  namespace :api do
    resources :users
  end
end
"#;
        let (syms, edges) = extract(src);
        let rid = routes_id(&syms).unwrap();
        let targets = route_targets(&edges, rid);
        // Controllers stored bare (no `Api::` prefix) — matches Ruby extractor convention.
        assert!(targets.contains(&"UsersController.index"), "got: {targets:?}");
    }

    #[test]
    fn member_block_adds_extra_action() {
        let src = r#"Rails.application.routes.draw do
  resources :users do
    member do
      get :preview
    end
  end
end
"#;
        let (syms, edges) = extract(src);
        let rid = routes_id(&syms).unwrap();
        let targets = route_targets(&edges, rid);
        assert!(targets.contains(&"UsersController.index"), "default action missing");
        assert!(targets.contains(&"UsersController.preview"),
            "expected member action UsersController.preview, got: {targets:?}");
    }

    #[test]
    fn collection_block_adds_extra_action() {
        let src = r#"Rails.application.routes.draw do
  resources :users do
    collection do
      get :search
    end
  end
end
"#;
        let (syms, edges) = extract(src);
        let rid = routes_id(&syms).unwrap();
        let targets = route_targets(&edges, rid);
        assert!(targets.contains(&"UsersController.search"),
            "got: {targets:?}");
    }

    #[test]
    fn parse_controller_action_handles_paths_and_classes() {
        assert_eq!(
            parse_controller_action("users#index"),
            Some(("UsersController".to_string(), "index".to_string()))
        );
        assert_eq!(
            parse_controller_action("admin/users#index"),
            Some(("UsersController".to_string(), "index".to_string()))
        );
        // Already class-form
        assert_eq!(
            parse_controller_action("UsersController#index"),
            Some(("UsersController".to_string(), "index".to_string()))
        );
        assert_eq!(parse_controller_action("nohash"), None);
        assert_eq!(parse_controller_action("#"), None);
    }

    #[test]
    fn full_routes_file_smoke() {
        let src = r#"Rails.application.routes.draw do
  root 'home#index'

  resources :users do
    member do
      get :avatar
    end
    collection do
      get :search
    end
  end

  resource :profile, only: [:show, :edit, :update]

  namespace :api do
    resources :posts, except: [:destroy]
  end

  get '/health' => 'system#health'
  get '/version', to: 'system#version'
end
"#;
        let (syms, edges) = extract(src);
        let rid = routes_id(&syms).unwrap();
        let targets = route_targets(&edges, rid);

        // root
        assert!(targets.contains(&"HomeController.index"));
        // resources :users — full 7
        assert!(targets.contains(&"UsersController.index"));
        assert!(targets.contains(&"UsersController.destroy"));
        // member/collection extras
        assert!(targets.contains(&"UsersController.avatar"));
        assert!(targets.contains(&"UsersController.search"));
        // singular resource :profile narrowed
        assert!(targets.contains(&"ProfilesController.show"));
        assert!(targets.contains(&"ProfilesController.edit"));
        assert!(!targets.contains(&"ProfilesController.create"));
        // namespaced resources :posts except destroy
        assert!(targets.contains(&"PostsController.index"));
        assert!(!targets.contains(&"PostsController.destroy"),
            "expected destroy excluded, got: {targets:?}");
        // verbs
        assert!(targets.contains(&"SystemController.health"));
        assert!(targets.contains(&"SystemController.version"));
    }
}
