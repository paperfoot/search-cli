---
name: search-cli-coding-research
description: use this when acting as an ai coding agent in opencode and external web knowledge is needed through the search-cli/opencode search tool. covers provider-aware query planning, efficient use of brave, browserless, exa, jina, and tavily, choosing search vs extract/scrape/similar operations, shaping exact/semantic/migration/security/release-note queries, minimizing quota waste, and deciding when to use the opencode wrapper versus direct cli fallback.
---

# Search CLI Coding Research

Use the OpenCode `search` tool as the default interface to the `search` binary. Use the CLI directly only when the OpenCode wrapper is unavailable, you need to debug the wrapper, or a required `search-cli` capability is hidden by the wrapper.

Assume the preferred configured providers are `brave`, `browserless`, `exa`, `jina`, and `tavily` unless `operation=providers` reports otherwise.

## Operating principle

Think first, then search the smallest unknown. A good call answers: "What exact external fact would unblock this code change?" Do not paste the whole user task into search.

Prefer one OpenCode tool call and one CLI-backed provider fanout. Use `query_plan=single` by default; reserve `query_plan=multi` for high-stakes ambiguity because the attached wrapper executes multiple CLI invocations for multi-plan.

## Default workflow

1. Inspect local repo context first: language, framework, package name, package version, failing command, exact error, and relevant config.
2. Choose the search path from the table below.
3. Call the OpenCode `search` tool with a narrow query, `count` 5-10, and `provider_policy=auto`.
4. Read returned `status`, `calls`, `results`, `provider_discovery`, and `next_actions`.
5. If a specific source matters, call `operation=extract` on the best URL before coding.
6. Cite or record external URLs when the answer depends on current external facts.

## Path selection

| Need | Tool args |
|---|---|
| Exact error, panic, build failure, stack trace | `operation=search`, `strategy=error_debugging`, `query_plan=single`, `providers=brave,jina`, include exact error plus package/version in `task_context` |
| Official API docs or config syntax | `strategy=official_docs`, `query_plan=single`, `providers=brave,exa,jina`; add `domains` only when the authoritative domain is known |
| Migration, breaking change, release notes | `strategy=migration` or `release_notes`, `freshness=year`, `providers=brave,exa,tavily`, `query_plan=single` unless multiple versions/frameworks are ambiguous |
| Security advisory, CVE, vulnerable dependency | `strategy=security`, `mode=news`, `freshness=month`, `providers=brave,tavily`, include package and version |
| Conceptual/API design question | `strategy=semantic` or `hyde`, `providers=exa,tavily`, `query_plan=single`; use natural-language wording, not keyword soup |
| Current ecosystem consensus or tradeoff | `strategy=step_back` or `hype`, `providers=exa,tavily`, `query_plan=single`; use `mode=deep` only if one pass is insufficient |
| Known URL needs reading | `operation=extract`, `query=<url>`, `providers=jina`, raise `max_snippet_chars` to 12000-20000 if needed |
| JS-heavy/protected page needs reading | `operation=scrape`, `query=<url>`, `providers=browserless`, larger `timeout_ms` |
| Similar pages from a known URL | `operation=similar`, `query=<url>`, `providers=exa` |
| Provider diagnostics | `operation=providers` first; `operation=config_check` only for setup failures |

## Provider heuristics

- **Brave**: use for exact keywords, error strings, official docs discovery, current web/news, and domain-restricted queries. Use concise queries with symbols, package names, quoted errors, `site:`-like domain restrictions through `domains`, and freshness filters.
- **Exa**: use for semantic discovery, conceptual docs, people/similar pages, and finding relevant pages when the exact keywords are unknown. Write natural-language or HyDE-style queries.
- **Tavily**: use for synthesis-oriented research, news/release/security checks, and broad research where a concise answer plus ranked sources is useful.
- **Jina**: use for fast URL-to-markdown extraction and as a lightweight web-search supplement. Use `operation=extract` once you have a URL.
- **Browserless**: use only for URL scraping when Jina/extract is insufficient due to JavaScript, bot protection, or rendered content.

## Query shaping rules

- Exact debugging: quote the invariant error text only. Add framework/package/version in `task_context`, not by bloating the query.
- Official docs: include the API/object/config name and desired task. Add one or two authoritative `domains` only when known.
- Semantic: write the query as the page you hope exists, e.g. “A technical document explaining how to migrate X from v1 to v2, including removed APIs and examples.”
- Release/migration: include old version, new version, package, and “migration guide”, “breaking changes”, or “release notes”.
- Security: include package name, version/range, “CVE”, “advisory”, “mitigation”, and use freshness.
- Avoid large domain lists, broad low-signal phrases, and generic questions like “how do I fix this app”.

## OpenCode call examples

```json
{
  "operation": "search",
  "query": "TypeError fetch failed undici ECONNRESET",
  "strategy": "error_debugging",
  "query_plan": "single",
  "providers": "brave,jina",
  "task_context": "Node.js 20, undici, failing integration test",
  "count": 8
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
  "count": 6
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
