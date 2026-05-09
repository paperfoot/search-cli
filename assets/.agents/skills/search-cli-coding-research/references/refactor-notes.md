# High-ROI Refactor Notes

These notes summarize recommended improvements found while reviewing the provided OpenCode wrapper and the search-cli source behavior.

## Wrapper recommendations
tool definition file: "C:\msys64\tmp\search-cli-fix\assets\.agents\tool\opencode\search.ts"

### 1. Remove or gate unsupported `you` provider

The wrapper declares provider `you`, env keys, categories, and capabilities, but the reviewed search-cli provider validation list does not include `you`. If `SEARCH_KEYS_YOU` is present, the wrapper can pass `-p you` and cause a CLI config error. Remove `you` from the wrapper until the CLI implements it, or populate provider metadata dynamically from `search agent-info --json`.

### 2. Derive provider/mode metadata from `search agent-info --json`

The wrapper duplicates provider capabilities, categories, env keys, and mode compatibility. This is useful for speed but creates drift risk. A better design is:

1. read env/config locally for zero-quota availability;
2. call `search agent-info --json` once per long TTL or on version change;
3. merge CLI-declared providers/modes with wrapper-only categories;
4. reject providers not present in the CLI manifest.

`agent-info` is local CLI metadata, not a provider API probe, so it should not consume search quota.

### 3. Make `query_plan=multi` explicit about cost and parallelize it

The current wrapper executes multi-plan as multiple sequential CLI invocations. That is useful but contradicts a strict “one CLI invocation” goal and adds latency. Improvements:

- rename current behavior to `query_plan=multi_invocation`, or report estimated invocation count before execution;
- execute independent CLI calls concurrently with `Promise.allSettled` when multi-plan is selected;
- add `query_plan=single_fanout` as the quota-default path.

### 4. Improve cooldown detection by exposing CLI failure detail

The wrapper expects `providers_failed_detail`, but the reviewed CLI response metadata exposes only provider names in `providers_failed`. Add structured details to CLI metadata, for example:

```json
{
  "provider": "brave",
  "code": "rate_limited",
  "message": "HTTP 429",
  "retryable": true
}
```

Then cooldowns can be accurate without parsing stderr or over-cooling transient failures.

### 5. Preserve cacheability when safe

The CLI query cache is used only when providers, domains, exclude domains, and freshness are absent. The wrapper often passes explicit providers and a denylist, so it bypasses the cache. Options:

- extend CLI cache keys to include providers/domains/freshness;
- let wrapper omit explicit providers when the CLI can safely skip unconfigured providers;
- add wrapper-level cache for identical normalized calls.

### 6. Add a single-process multi-query CLI mode

For coding agents, the ideal high-quality path is one tool call, one CLI process, multiple provider-backed subqueries. Add a command such as:

```bash
search batch --json <<'JSON'
[
  {"query":"... exact ...","mode":"general","providers":["brave","jina"],"count":5},
  {"query":"... semantic ...","mode":"general","providers":["exa","tavily"],"count":5}
]
JSON
```

The CLI could reuse clients, run subqueries in parallel, dedupe globally, and expose per-call metadata. This would outperform sequential wrapper-managed multi-plan.

### 7. Avoid fastest-provider bias in CLI result merging

The CLI currently collects provider results as tasks complete and can abort slower providers once enough results are gathered. This is fast, but with small `count` values it can favor faster providers over better providers. Consider:

- collect at least one result batch from each selected provider before truncating;
- use a short grace period before aborting slow providers;
- score/rank by provider/category, exact match, domain authority, freshness, and duplicate agreement;
- allocate per-provider result budgets before global truncation.

### 8. Add query-plan dry run

Expose wrapper/CLI planning without consuming provider searches:

```json
{
  "operation": "plan",
  "query": "...",
  "strategy": "migration",
  "providers": "brave,exa,tavily"
}
```

Return shaped query, provider set, mode, estimated CLI invocations, and cacheability. This helps agents inspect cost before expensive research.

### 9. Make Browserless endpoint configurable

The Browserless provider uses a fixed cloud endpoint. Prefer config/env support for endpoint/region, since Browserless deployments often vary by account or region.

## Agent behavior improvements independent of refactors

- Default to `query_plan=single` and a small provider list.
- Use `operation=extract` on the top official URL instead of broad re-searching.
- Use exact, semantic, or synthesis strategies intentionally; do not mix all three unless the task is truly ambiguous.
- Keep `count` moderate. Asking each provider for 20-50 results is rarely useful for coding changes.
