# Query Playbook

## Goal

Get the highest-quality external evidence with the fewest quota-consuming searches. Optimize for one well-shaped OpenCode tool call that fans out to a small, compatible provider set.

## Pre-search checklist

Before searching, identify as many of these as possible from local files:

- language/runtime and version
- package/framework/library and version
- exact error/panic/log line
- target API, class, function, config key, or CLI flag
- OS/platform/build tool if relevant
- whether the answer must be current
- whether official documentation is required

If the local repo already contains enough information, do not search.

## Search patterns

### 1. Exact error debugging

Use when the unknown is a concrete failure.

Recommended tool args:

```json
{
  "operation": "search",
  "strategy": "error_debugging",
  "query_plan": "single",
  "providers": "brave,jina",
  "count": 5,
  "query": "<short invariant error text>",
  "task_context": "<package/framework/version + failing command>"
}
```

Rules:

- Keep only the stable part of the error.
- Remove local paths, IDs, random hashes, and machine-specific values unless they are the error.
- Use `freshness=year` only for version-specific errors or recent package releases.
- If the first result is a doc or issue with the likely answer, call `operation=extract` on that URL instead of more broad searching.

### 2. Official documentation

Use when implementation correctness depends on current API syntax or supported behavior.

Recommended tool args:

```json
{
  "operation": "search",
  "strategy": "official_docs",
  "query_plan": "single",
  "providers": "brave,exa,jina",
  "domains": "<official domain if known>",
  "count": 5,
  "query": "<package/API/config name> <specific task>"
}
```

Rules:

- Add `domains` only when known. One good authoritative domain beats five guesses.
- Do not over-restrict domain for package ecosystems with docs split across multiple domains.
- Extract the official page before relying on details.

### 3. Migration and release changes

Use when upgrading dependencies, fixing deprecations, or checking breaking changes.

Recommended tool args:

```json
{
  "operation": "search",
  "strategy": "migration",
  "freshness": "year",
  "query_plan": "single",
  "providers": "brave,exa,tavily",
  "count": 8,
  "query": "<package> <old version> to <new version> <API/topic>"
}
```

Rules:

- Include both versions if known.
- Prefer release notes, migration guides, official changelogs, and maintainers' issues over blog posts.
- Use `query_plan=multi` only if the first pass mixes unrelated versions or ecosystems.

### 4. Security and dependency risk

Use when security implications may change the implementation or dependency version.

Recommended tool args:

```json
{
  "operation": "search",
  "strategy": "security",
  "mode": "news",
  "freshness": "month",
  "query_plan": "single",
  "providers": "brave,tavily",
  "count": 8,
  "query": "<package> <version> CVE advisory vulnerability mitigation"
}
```

Rules:

- Prefer official advisories, vendor notices, NVD, GitHub Security Advisories, and release notes.
- Include affected version and target patched version in the final reasoning.

### 5. Semantic discovery

Use when the precise terminology is unknown or the target is conceptual.

Recommended tool args:

```json
{
  "operation": "search",
  "strategy": "hyde",
  "query_plan": "single",
  "providers": "exa,tavily",
  "count": 8,
  "query": "<natural-language description of desired page or answer>"
}
```

Rules:

- Write a query that resembles the document you hope to find.
- Avoid `site:`-style restriction unless you know the official domain.
- If results identify the right vocabulary, do a second narrower search only if needed.

### 6. Extraction

Use once a URL has been selected.

Recommended tool args:

```json
{
  "operation": "extract",
  "query": "<url>",
  "providers": "jina",
  "max_snippet_chars": 16000
}
```

Fallback:

```json
{
  "operation": "scrape",
  "query": "<url>",
  "providers": "browserless",
  "timeout_ms": 120000,
  "max_snippet_chars": 20000
}
```

Use Browserless only after Jina/extract fails or returns rendered-empty content.

## Provider selection with enabled providers

Assuming `brave,browserless,exa,jina,tavily`:

- `brave,jina`: exact errors, official docs discovery, keyword lookup.
- `exa,tavily`: semantic API discovery, architecture/design research, ambiguous concepts.
- `brave,exa,tavily`: migration/release/security research when both exact and semantic evidence matter.
- `jina`: known URL extraction.
- `browserless`: JS-heavy/protected URL scraping.

## Count and freshness defaults

- `count=5`: exact errors, official docs, known narrow question.
- `count=8-10`: migrations, release notes, ambiguous API behavior.
- `count=15+`: only for broad landscape research.
- `freshness=none`: stable APIs and concepts.
- `freshness=year`: migrations, releases, recent framework behavior.
- `freshness=month`: security or current breakage.
- `freshness=week/day`: news, outages, very recent regressions.

## Stop conditions

Stop searching and start extracting/implementing when:

- an official URL directly addresses the unknown;
- two independent high-quality sources agree;
- results are repetitive and no new evidence appears;
- the remaining uncertainty is local-code-specific rather than web-specific.

If results are low quality, do not keep retrying the same shape. Change one of: strategy, provider set, domain restriction, exactness, or freshness.
