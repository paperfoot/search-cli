---
name: search-cli-coding-research
description: use this when acting as an ai coding agent in opencode and external web knowledge is needed through the search-cli/opencode search tool. covers provider-aware query planning, efficient use of brave, browserless, exa, jina, tavily, and you, choosing search vs extract/scrape/similar operations, shaping exact/semantic/migration/security/release-note queries, minimizing quota waste, and deciding when to use the opencode wrapper versus direct cli fallback.
---

# Search CLI Coding Research

Use the OpenCode `search` tool as the default interface to the `search` binary. Use the CLI directly only when the OpenCode wrapper is unavailable, you need to debug the wrapper, or a required `search-cli` capability is hidden by the wrapper.

Assume the preferred configured providers are `brave`, `browserless`, `exa`, `jina`, `tavily`, and `you` unless `operation=providers` reports otherwise.

## Operating principle

Think first, then search the smallest unknown. A good call answers: "What exact external fact would unblock this code change?" Do not paste the whole user task into search.

**Hard rules for every search call:**

1. **Single-plan by default.** Use `query_plan=single`. Reserved `query_plan=multi` only for truly ambiguous needs (e.g., researching multiple incompatible versions simultaneously). Multi-plan creates up to 3 CLI invocations — use it only when single-plan cannot answer the question.
2. **One strategy per call.** Pick the ONE strategy that matches your research need. Do not mix `error_debugging` + `official_docs` + `semantic` in one `query_plan=multi` call. Use the path selection table below to choose.
3. **Moderate count.** Keep `count` at 5-10. Asking for 50 results wastes quota, saturates the snippet budget, and does not improve answer quality.
4. **Extract first, always.** After every search, inspect `next_actions`. If it suggests extracting a specific URL, call `operation=extract` on that URL IMMEDIATELY — before any re-search or coding. This single pattern saves more quota than any other tactic.
5. **Check current_date.** The tool response includes a top-level `current_date` field (YYYY-MM-DD format) and `tool.current_date`. Read this date and incorporate it into your query to avoid targeting outdated years. Anti-pattern: searching "latest React 2024" when the actual date is 2026-05-09 — this returns stale results. Correct pattern: use the `current_date` value to date-stamp your query so you target current information.

## Default workflow

1. Inspect local repo context first: language, framework, package name, package version, failing command, exact error, and relevant config.
2. Choose the search path from the table below. Pick ONE strategy that matches your need.
3. Call the OpenCode `search` tool with a narrow query, `query_plan=single`, `count` 5-10, and `provider_policy=auto`.
4. Read returned `status`, `calls`, `results`, `provider_discovery`, and **especially `next_actions`**.
5. **If `next_actions` suggests an extraction URL, call `operation=extract` on that URL FIRST.** Do not re-search or start coding until you have read the best source. This is the single highest-ROI pattern in the entire tool.
6. Only if extraction fails or produces insufficient information, consider a second search with a different strategy or `query_plan=multi`.
7. Cite or record external URLs when the answer depends on current external facts.

## Strategy discipline

| Your need | Use this strategy | Do NOT mix with |
|---|---|---|
| Exact error, panic, stack trace, build failure | `error_debugging` | semantic, synthesis |
| API usage, config syntax, "how to implement X" | `official_docs` | semantic, hyde |
| Package version migration, breaking changes | `migration` or `release_notes` | error_debugging |
| Security advisory, CVE, vulnerable dep | `security` | official_docs |
| Conceptual understanding, "what is the best way to" | `semantic` or `hyde` | error_debugging, exact |
| Ecosystem consensus, tradeoffs | `step_back` or `hype` | exact |
| Academic paper or formal research | `academic` | error_debugging |

Pick ONE row. Shape your query accordingly. If you need both an error fix AND official docs, make two separate `query_plan=single` calls — not one `query_plan=multi`.

## Strategy × Mode × Freshness triplets

This table combines strategy, mode, and freshness for the 8 most common research patterns. Use it instead of guessing mode/freshness separately.

| Need | Strategy | Mode | Freshness | Providers | Count |
|---|---|---|---|---|---|
| Exact error message / stack trace | `error_debugging` | `auto` | `none` | `brave,jina` | 5 |
| Official API docs / how-to | `official_docs` | `auto` | `none` | `brave,exa,jina` | 5 |
| Package version migration | `migration` | `auto` | `year` | `brave,exa,tavily` | 8 |
| Security advisory / CVE | `security` | `news` | `month` | `brave,tavily` | 5 |
| Release notes / changelog | `release_notes` | `auto` | `year` | `brave,tavily` | 5 |
| Conceptual design / architecture | `semantic` | `auto` | `none` | `exa,tavily` | 5 |
| Ecosystem consensus / tradeoffs | `step_back` | `auto` | `none` | `exa,tavily` | 5 |
| Academic paper / formal research | `academic` | `scholar` | `none` | `exa,serpapi` | 5 |

## Path selection

| Need | Tool args |
|---|---|
| Exact error, panic, build failure, stack trace | `operation=search`, `strategy=error_debugging`, `query_plan=single`, `providers=brave,jina`, `count=5`, include exact error plus package/version in `task_context` |
| Official API docs or config syntax | `strategy=official_docs`, `query_plan=single`, `providers=brave,exa,jina`, `count=5`; add `domains` only when the authoritative domain is known |
| Migration, breaking change, release notes | `strategy=migration` or `release_notes`, `freshness=year`, `providers=brave,exa,tavily`, `query_plan=single`, `count=8` unless multiple versions/frameworks are ambiguous |
| Security advisory, CVE, vulnerable dependency | `strategy=security`, `mode=news`, `freshness=month`, `providers=brave,tavily`, `query_plan=single`, `count=5`, include package and version |
| Conceptual/API design question | `strategy=semantic` or `hyde`, `providers=exa,tavily`, `query_plan=single`, `count=5`; use natural-language wording, not keyword soup |
| Current ecosystem consensus or tradeoff | `strategy=step_back` or `hype`, `providers=exa,tavily`, `query_plan=single`, `count=5`; use `mode=deep` only if one pass is insufficient |
| Known URL needs reading | `operation=extract`, `query=<url>`, `providers=jina`, `count=1`, raise `max_snippet_chars` to 12000-20000 if needed |
| JS-heavy/protected page needs reading | `operation=scrape`, `query=<url>`, `providers=browserless`, `count=1`, larger `timeout_ms` |
| Similar pages from a known URL | `operation=similar`, `query=<url>`, `providers=exa`, `count=5` |
| Provider diagnostics | `operation=providers` first; `operation=config_check` only for setup failures |

## Provider heuristics

- **Brave**: use for exact keywords, error strings, official docs discovery, current web/news, and domain-restricted queries. Use concise queries with symbols, package names, quoted errors, `site:`-like domain restrictions through `domains`, and freshness filters.
- **Exa**: use for semantic discovery, conceptual docs, people/similar pages, and finding relevant pages when the exact keywords are unknown. Write natural-language or HyDE-style queries.
- **Tavily**: use for synthesis-oriented research, news/release/security checks, and broad research where a concise answer plus ranked sources is useful.
- **Jina**: use for fast URL-to-markdown extraction and as a lightweight web-search supplement. Use `operation=extract` once you have a URL.
- **Browserless**: use only for URL scraping when Jina/extract is insufficient due to JavaScript, bot protection, or rendered content.
- **You**: use for synthesis, broad research, and current-awareness queries. Good fallback when Tavily or Perplexity is unavailable. Supports keyword and synthesis categories.

## Response field interpretation

| Field | What it tells you | Action |
|---|---|---|
| `status` | `success` = results available; `partial_success` = some providers returned results, some failed — use what you have, do NOT re-search; `no_results` = check `provider_discovery.hidden_cooldown_count` — if >0, providers are cooling down, either wait or try different providers; `all_providers_failed` = run `operation=config_check` to verify setup; `error` = inspect `error.code` |
| `next_actions` | Wrapper-generated suggestions | **Always check this first.** If it suggests an extraction URL, call `operation=extract` IMMEDIATELY |
| `provider_discovery.configured` | Active provider list | If empty or missing expected providers, run `operation=providers` or `operation=config_check` |
| `calls` | Per-invocation shaped query, mode, providers, strategy | Verify the wrapper picked the right strategy and providers for your need |
| `results` | Deduped normalized results | Use directly. If insufficient, follow `next_actions` or try a different strategy |

## Query shaping rules

- Exact debugging: quote the invariant error text only. Add framework/package/version in `task_context`, not by bloating the query.
- Official docs: include the API/object/config name and desired task. Add one or two authoritative `domains` only when known.
- Semantic: write the query as the page you hope exists, e.g. "A technical document explaining how to migrate X from v1 to v2, including removed APIs and examples."
- Release/migration: include old version, new version, package, and "migration guide", "breaking changes", or "release notes".
- Security: include package name, version/range, "CVE", "advisory", "mitigation", and use freshness.
- Avoid large domain lists (max 5), broad low-signal phrases, and generic questions like "how do I fix this app".
- **Date-stamp your query with `current_date`.** Do not hard-code a specific year (e.g., "2024" or "2025") unless you have confirmed it is the current year. Read `current_date` from the tool output and use it in your query to target current information. Anti-pattern: "latest Node.js 2024 docs" when `current_date` is 2026-05-09 — this misses 2025/2026 releases. Correct pattern: "latest Node.js [current_date] documentation" where you substitute the actual date value.

## Quota awareness

Each search tool call consumes provider API quota:

- **`query_plan=single`**: 1 CLI invocation, typically 2-3 provider API calls (the wrapper selects configured providers from your chosen category).
- **`query_plan=multi`**: up to 3 CLI invocations, each with its own provider set — can consume 6-9 provider calls.
- **`operation=extract`**: 1 CLI invocation, 1 provider call (jina, browserless, or stealth).
- **`operation=providers` / `config_check`**: zero provider quota — these are local CLI diagnostics.

**Target**: ≤ 3 search tool calls per coding session. Prefer extraction over re-searching whenever `next_actions` suggests it.

## OpenCode call examples

```json
{
  "operation": "search",
  "query": "TypeError fetch failed undici ECONNRESET",
  "strategy": "error_debugging",
  "query_plan": "single",
  "providers": "brave,jina",
  "task_context": "Node.js 20, undici, failing integration test",
  "count": 5
}
```

```json
{
  "operation": "search",
  "query": "React Router v6 loader redirect API",
  "strategy": "official_docs",
  "query_plan": "single",
  "providers": "brave,exa,jina",
  "domains": "reactrouter.com",
  "count": 5
}
```

```json
{
  "operation": "extract",
  "query": "https://example.com/official-doc-page",
  "providers": "jina",
  "max_snippet_chars": 16000
}
```

For more detailed routing, wrapper behavior, and refactor notes, consult:

- `references/query-playbook.md`
- `references/opencode-tool-contract.md`
- `references/refactor-notes.md`