# CLAUDE.md template — ast-graph integration

Drop the snippet below into your project's `CLAUDE.md` (creating one if needed) so Claude leverages ast-graph for code intelligence on this repo. Adjust the customization markers (`<<…>>`) to match your project.

---

## Code intelligence — use ast-graph for structural questions

This repository is indexed by [ast-graph](https://github.com/emtyty/ast-graph). The graph is stored at **`<repo-root>/.ast-graph/graph.db`** (per-repo, sitting next to the code — this is the CLI's default when run from the repo root). It contains every function, class, method, import, and call edge across the codebase. **Prefer ast-graph over grep / read for whole-codebase structural questions** — faster and gives exact counts instead of grep false-positives.

Add `.ast-graph/` to `.gitignore` — the DB is binary, churn-heavy, and recreatable from source.

### Activation triggers — reach for ast-graph FIRST when

The user's intent matches any of these patterns. Run the relevant ast-graph command **before** opening files or grepping:

| Intent | Trigger phrases | First commands |
|---|---|---|
| **Onboarding** | "understand this repo", "what is this project", "give me a tour", "where do I start", "explain the codebase" | `ast-graph stats` → `ast-graph hotspots --limit 20` |
| **Codebase analysis** | "analyze the codebase", "map the architecture", "find structural issues", "audit the project" | `ast-graph stats` → `ast-graph hotspots` → `ast-graph dead-code` |
| **Understand a feature/symbol** | "how does X work", "what does X do", "explain X", "trace X" | `ast-graph symbol X` → `ast-graph call-chain X --depth 3` |
| **Impact / refactor planning** | "if I change X", "what depends on X", "is it safe to rename/delete X" | `ast-graph blast-radius X --depth 3 --with-recency` |
| **PR / diff review** | "review this branch", "what changed", "check the diff" | `ast-graph changed-symbols --base origin/main --callers` |
| **Find similar patterns** | "how is X done elsewhere", "examples of Y in this repo" | `ast-graph hotspots` → `ast-graph symbol SimilarThing --callees` |

If you're about to grep the whole repo or open >3 files just to "get oriented," **stop and run ast-graph first**.

For any of the intents above, the preferred path is to **delegate to the [`ql-ast-grapher`](#preferred-delegate-to-the-ql-ast-grapher-agent) sub-agent** rather than running the commands inline — see the Sub-agent delegation section below.

### Bootstrap (run once, or re-run after major refactors)

`cd` into the repo root (so the CLI's default DB path resolves) and run:

```bash
ast-graph scan .
```

That writes `<repo-root>/.ast-graph/graph.db`. The scan is incremental on subsequent runs — only changed files re-parsed. From outside the repo, pass `--db <repo-root>/.ast-graph/graph.db` explicitly.

### When to use ast-graph

> All commands below assume you're running from the repo root so the default DB path resolves. From outside the repo, prepend `--db <repo-root>/.ast-graph/graph.db` to each command.

| Question | Command |
|---|---|
| Who calls `X`? | `ast-graph symbol X --callers` |
| What does `X` call? | `ast-graph symbol X --callees` |
| Find a symbol by partial name | `ast-graph symbol "PartialName"` |
| If I change `X`, what breaks? | `ast-graph blast-radius X --depth 3` |
| Same, weighted by recent churn | `ast-graph blast-radius X --with-recency` |
| Trace a call chain from `X` | `ast-graph call-chain X --depth 3` |
| Map a PR's impact | `ast-graph changed-symbols --base origin/main --callers` |
| Find dead methods | `ast-graph dead-code --kinds Function,Method` |
| Architectural hotspots | `ast-graph hotspots --limit 20` |
| Whole-repo summary | `ast-graph stats` |
| Custom SQL query | `ast-graph query "SELECT …"` |

### When NOT to use ast-graph
- Reading or editing a specific file → use `Read` / `Edit`
- Searching for a string literal, comment, or config key → use grep
- Anything that needs to see code **inside** a function body in detail (the graph compresses bodies down to call edges only)

### Conventions
- Symbol names are stored qualified: **`ClassName.methodName`** (dot, not `::` or `#`).
- The graph is a static snapshot — re-scan after a major refactor with `ast-graph scan .` from the repo root (incremental, only changed files re-parsed).
- **Dynamic dispatch is invisible** to the graph: framework hooks, runtime metaprogramming, JS callbacks, and reflection-based calls won't appear as edges. Treat `dead-code` output as a candidate list, not a deletion list.

### Workflow expectations

When working on this repo, **before** taking any of these actions, consult the graph:

1. **Onboarding to the repo (first contact)** — start with `ast-graph stats` for size + language mix, then `ast-graph hotspots --limit 20` to surface the architectural centers, then `ast-graph symbol <top-hotspot>` to expand one. Three commands beat browsing the file tree blind.
2. **Analyzing the codebase** — combine `ast-graph stats` (totals), `ast-graph hotspots` (centers of gravity), `ast-graph dead-code` (unreferenced candidates), and a custom `ast-graph query` for any project-specific shape (controllers, jobs, handlers). Build the structural map first; read code to confirm only the parts that matter.
3. **Understanding a feature or symbol** — `ast-graph symbol X` for definition + immediate neighbors, then `ast-graph call-chain X --depth 3` to follow the flow downward, then `ast-graph blast-radius X --depth 3` to see who depends on it. Read the actual file only after you know the shape.
4. **Renaming / changing a method signature** — run `ast-graph blast-radius MyClass.method` first. List every caller; verify each still works after the change.
5. **Deleting a method** — `ast-graph symbol MyClass.method --callers`. Zero callers AND not in any of the dynamic-dispatch caveats above = safe to delete.
6. **Investigating a bug** — trace upward from the failing call: `ast-graph blast-radius FailingMethod` to find every entry point that reaches it.
7. **Reviewing a PR / diff** — `ast-graph changed-symbols --base origin/main --callers` shows exactly which symbols changed and who depends on them. Faster than scrolling the diff.
8. **Writing a new feature** — search for similar existing patterns: `ast-graph hotspots` + `ast-graph symbol SimilarFeature --callees` to learn the established shape before writing.

### Onboarding recipe — first 60 seconds in an unfamiliar repo

```bash
cd <repo-root>

# 1. Size + language mix
ast-graph stats

# 2. Architectural centers — what to read first
ast-graph hotspots --limit 20

# 3. Expand the top hotspot to see its members + neighbors
ast-graph symbol <name-from-step-2>

# 4. Project-specific entry points (HTTP, jobs, CLI commands)
#    Use the queries in "Project-specific entry points" below.
```

Report the findings to the user as a structural map (file count, language mix, top hotspots, entry points) **before** diving into specific files.

### Preferred: delegate to the `ql-ast-grapher` agent

A purpose-built sub-agent — **`ql-ast-grapher`** — wraps every workflow above (onboarding, analysis, understand-symbol, refactor planning, PR review, dead-code) and runs the ast-graph commands autonomously. It already knows the cheat sheet, the qualified-name convention, and the dynamic-dispatch caveat — you don't need to re-explain any of it.

**Use it as the default for structural tasks.** Spawn it via the Task tool with `subagent_type: "ql-ast-grapher"`.

#### When to delegate to `ql-ast-grapher`

| User intent | Delegate? | Prompt hint |
|---|---|---|
| Onboarding to an unfamiliar repo | **Yes** | "Map the structure of this repo. Report stats + top hotspots + entry points." |
| Codebase analysis / audit | **Yes** | "Run a structural audit: stats, hotspots, dead-code, top god-classes." |
| "How does X work?" | **Yes** | "Explain feature X. Use symbol → call-chain → blast-radius, then read the file." |
| "If I rename/delete X what breaks?" | **Yes** | "Run blast-radius on X with --with-recency. Group callers by file." |
| "What changed on this branch?" | **Yes** | "Run changed-symbols --base origin/main --callers. Highlight breaking-risk symbols." |
| Find similar existing patterns | **Yes** | "Find symbols similar to X via hotspots + symbol --callees." |
| Editing a specific known file | No | The grapher is read-only — use a coding agent and read the file directly. |
| Searching for a string literal / config key | No | Use grep. The graph doesn't index in-body strings. |

#### Minimal delegation prompt

```
Task: <one-sentence goal>
Repo root: <absolute path>   (DB at <repo-root>/.ast-graph/graph.db — CLI default)
Starting command: ast-graph <subcommand> <args>
Deliverable: <structural map | impact list | call chain | etc.>
```

The agent will `cd` to the repo root, bootstrap the DB if missing, run the commands, and return a structured report ending with `Status: DONE | DONE_WITH_CONCERNS | BLOCKED | NEEDS_CONTEXT`.

### Sub-agent delegation — pass ast-graph context explicitly (fallback)

When the task is **not** purely structural — e.g. a `researcher`, `planner`, `code-reviewer`, `debugger`, `scout`, or `fullstack-developer` doing work that involves both graph queries *and* something else (writing code, web research, running tests) — `ql-ast-grapher` is the wrong tool. Instead, delegate to the appropriate specialist agent and **explicitly include ast-graph instructions in its prompt** so it doesn't fall back to grep/read.

#### When to pass ast-graph to a sub-agent

| Sub-agent | Pass ast-graph? | Why |
|---|---|---|
| `researcher` exploring this repo | **Yes** | Hotspots + symbol lookup beat blind grepping |
| `planner` scoping a refactor | **Yes** | Blast-radius is the planning input |
| `code-reviewer` reviewing a diff | **Yes** | `changed-symbols --callers` is faster than reading the diff |
| `debugger` tracing a failing call | **Yes** | Blast-radius shows every entry point reaching the bug |
| `scout` / `Explore` for structural lookup | **Yes** | One ast-graph query replaces N greps |
| `Explore` for a specific filename / string literal | No | Plain grep is fine |
| `tester` running existing tests | No | No structural query needed |
| `git-manager` committing | No | No structural query needed |

#### Delegation prompt template

Paste this block into any sub-agent prompt that should use ast-graph:

```
This repo is indexed by ast-graph. The DB lives next to the code at:
  <repo-root>/.ast-graph/graph.db   (the CLI's default when run from repo root)

`cd` into the repo root, then run commands without --db. Before grep/read
for whole-codebase questions, run:
  ast-graph <command>

Cheat sheet:
  symbol <name>                       → definition + callers + callees
  call-chain <name> --depth 3         → trace downward
  blast-radius <name> --depth 3       → trace upward (impact)
  changed-symbols --base origin/main  → diff → symbols
  hotspots --limit 20                 → architectural centers
  dead-code                           → unreferenced functions/methods
  stats                               → repo summary
  query "<SQL>"                       → arbitrary SQLite query

Symbols are stored qualified: "ClassName.methodName" (dot, not :: or #).
Dynamic dispatch (callbacks, reflection, framework hooks) is invisible —
treat dead-code as a candidate list, not a deletion list.
```

#### Pre-delegation checklist (for the orchestrator)

Before spawning a sub-agent for a structural task:

1. Confirm the DB exists: `ls <repo-root>/.ast-graph/graph.db` — if missing, run `ast-graph scan .` from the repo root first.
2. Include the cheat sheet above in the sub-agent's prompt.
3. Tell the sub-agent the **specific** ast-graph command(s) to start with (don't make it guess).
4. For multi-agent parallel work, pass each sub-agent its own scoped query so they don't duplicate work.

### Project-specific entry points

<<EDIT THIS SECTION FOR YOUR REPO. Examples below.>>

- **HTTP entry points**: `ast-graph query "SELECT name, file_path FROM nodes WHERE name LIKE '%Controller.%' AND kind='Method'"`
- **Background jobs**: `ast-graph query "SELECT name FROM nodes WHERE kind='Class' AND signature LIKE '%Job%'"`
- **Test helpers**: skip when computing dead-code (`ast-graph dead-code --kinds Function,Method` already excludes vendored paths; add project-specific filters as needed)

