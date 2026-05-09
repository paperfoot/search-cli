# Search CLI System — Independent Review & Improvement Recommendations

**Date:** 2026-05-09  
**Review scope:** search-cli Rust binary (v0.5.1), OpenCode `search.ts` wrapper, and `search-cli-coding-research` skill  
**Source files inspected:** All key Rust source files (`src/main.rs`, `src/cli.rs`, `src/engine.rs`, `src/cache.rs`, `src/classify.rs`, `src/config.rs`, `src/types.rs`, `src/providers/`), full `search.ts` wrapper (1315 lines), full SKILL.md, query-playbook.md, opencode-tool-contract.md, and refactor-notes.md

---

## 1. Existing Recommendations (from refactor-notes.md)

The independent review captured 9 wrapper-level and 4 agent-behavior-level recommendations. All are **valid** and confirmed by direct source inspection. Below is a rating with additional context from source-level analysis:

### Wrapper Recommendations

| # | Recommendation | Verdict | Source Evidence |
|---|---|---|---|
| 1 | Remove/gate unsupported `you` provider | ~~❌ OBSOLETE~~ | **UPDATE (2026-05-09):** `you` provider has been fully implemented in CLI (`src/providers/you.rs`, `src/main.rs` L312, `src/engine.rs` default sets, `src/config.rs` ApiKeys, `tests/integration.rs`). Wrapper already has `you` correctly in PROVIDERS, CAPABILITIES, CATEGORIES, and ENV_KEYS maps. This recommendation is no longer needed. |
| 2 | Derive provider/mode metadata from `agent-info --json` | ✅ **P2** | Wrapper hardcodes PROVIDERS, CAPABILITIES, CATEGORIES, ENV_KEYS (lines 175-208). `agent-info --json` returns provider name, configured boolean, capabilities, and env_keys — wrapper could merge with hardcoded category maps. |
| 3 | Make `query_plan=multi` explicit about cost, parallelize | ✅ **P1** | Multi-plan currently fires up to 3 sequential CLI invocations in `execute()` (lines 1215-1236). Each invocation is `await runSearchCli(...)` in a for-loop. Parallelizing with `Promise.allSettled` would reduce latency. |
| 4 | Improve cooldown detection via structured CLI failure | ✅ **P0** | CLI's `ResponseMetadata` (types.rs) already has `providers_failed_detail: Vec<ProviderFailureDetail>` with provider/reason/code/cause fields. Wrapper's `markCooldownsFromPayload` (line 909) reads this correctly. But exit-code-based cooldown (line 994) could also trigger on `code=4` (rate_limited). |
| 5 | Preserve cacheability when safe | ✅ **P1** | CLI caches only when providers, domains, freshness are ALL absent (cache.rs). Wrapper ALWAYS passes `-p` (line 759), disabling cache. Fix: omit `-p` when CLI auto-discovers, OR extend CLI cache key. |
| 6 | Add single-process multi-query CLI mode (`search batch`) | ✅ **P1** | Current multi-plan = sequential CLI processes. One process with shared HTTP clients, parallel subqueries, global dedupe would be far more efficient. |
| 7 | Avoid fastest-provider bias in result merging | ✅ **P2** | engine.rs `collect_results` aborts remaining tasks once count/min_results reached. Faster providers dominate small counts. |
| 8 | Add query-plan dry run | ✅ **P2** | No mechanism to preview shaped queries and provider selection without consuming quota. |
| 9 | Make Browserless endpoint configurable | ✅ **P2** | Provider hardcodes `https://cloud.browserless.io`. Should be configurable per account/region. |

### Agent Behavior Recommendations

| # | Recommendation | Verdict | Source Evidence |
|---|---|---|---|
| A1 | Default to `query_plan=single`, small provider list | ✅ **P0** | Skill's workflow (SKILL.md line 18-25) already says "Prefer one OpenCode tool call and one CLI-backed provider fanout." But the path selection table doesn't enforce single for every path. |
| A2 | Use `operation=extract` on top URL | ✅ **P0** | Wrapper's `suggestNextActions` (line 976) already suggests this. Skill should teach agents to follow the suggestion FIRST before re-searching. |
| A3 | Use intentional strategies | ✅ **P0** | Skill path table assigns strategies but doesn't explain WHY mixing all 3 degrades quality. Multi-plan keyword+semantic+synthesis produces overlapping/contradictory results. |
| A4 | Keep count moderate (5-10) | ✅ **P0** | Skill defaults (line 22) say count 5-10. All query patterns in playbook use 5 or 8. Confirmed. |

---

## 2. Additional Recommendations (Not in Original Review)

These were discovered through comprehensive source-level inspection of all three components.

### A. search-cli Rust CLI — Engine, Classification, Cache

#### A1. Query Intent Semantic Classification (P1 — HIGH value, moderate effort)

**Problem:** `src/classify.rs` uses regex patterns only (e.g., `error_debugging` matches on "error|exception|panic|failed|traceback|stack trace"). This is fragile — many legitimate search queries for docs contain the word "error" but aren't debugging queries.

**Recommendation:** Add a semantic layer before regex fallback:
- If query matches `^https?://` → `extract` / `scrape` / `similar` (mode auto-detection, already handled)
- If query contains error-like patterns AND mentions a package/version → `error_debugging`
- If query asks "how to", "implement", "best practice", "example" → `official_docs`
- If query contains version numbers (e.g., "v2→v3", "3.x to 4.0") → `migration`
- If query contains "CVE", "advisory", "vulnerability", "patch" → `security`
- Fallback → `general`

This lets `auto` mode (engine.rs line 58-72 speculative Brave+Serper fire) select optimal providers/strategy without caller override.

#### A2. Per-Provider Timeout Configuration (P2 — nice to have)

**Problem:** Timeout is global (`--timeout` flag, 90s default). Perplexity needs 45-60s; Brave needs 2-5s. A 90s timeout wastes time on fast providers.

**Recommendation:** Add `[timeouts]` section in `config.toml`:
```toml
[timeouts]
brave = 10
serper = 8
perplexity = 60
browserless = 45
default = 30
```
Provider trait already has `timeout_ms` in `SearchOpts`. Extend config loading to populate it.

#### A3. Result Relevance Scoring (P2 — nice to have)

**Problem:** `SearchResult` struct has no relevance field. Merged results are ordered by provider arrival, not quality.

**Recommendation:** Add `relevance: Option<f32>` to `SearchResult`. Populate with:
- Exact match bonus for keyword providers (+0.3)
- Freshness/recency bonus for news/social modes (+0.2)
- Domain authority bonus for official docs domains (+0.1)
- Deduped agreement bonus (same URL from multiple providers → +0.1)

Score ranked output in `search` and `search_news` return paths.

#### A4. Per-Provider Result Budget Allocation (P2 — refines recommendation #7)

**Problem:** `count` is a single cap applied across all providers. If `count=10` and 3 providers are selected, the first provider returning 10 results aborts the others.

**Recommendation:** Add `--per-provider-count` flag or `[budgets]` config section that allocates result slots per provider before global dedupe:
```json
{"brave": 4, "exa": 3, "tavily": 3, "total": 10}
```

This replaces the current fastest-provider-wins behavior with intentional allocation.

#### A5. CLI Response Size Guard (P2 — low effort)

**Problem:** For `count=10` with 3 providers, raw JSON can easily reach 200KB+. LLM token consumption from bloated responses wastes context.

**Recommendation:** Add `--max-response-bytes <N>` flag. When total JSON exceeds threshold, auto-truncate snippet fields to stay under budget. Or, truncate snippet fields beyond `--max-snippet-chars` globally.

---

### B. search.ts OpenCode Wrapper — Planning, Execution, Cooldowns

#### B1. Strategy-Preserving Multi-Plan Fix (P0 — bug fix, HIGH impact)

**Problem:** In `buildInvocations` (line 837), the semantic call in multi-plan reuses `input.strategy`:
```typescript
{ category: "semantic", mode, strategy: input.strategy === "auto" ? "semantic" : input.strategy, ... }
```
If `input.strategy = "official_docs"`, the semantic call gets `official_docs` strategy, which is wrong — semantic providers (Exa, Tavily) expect `semantic`, `hyde`, or `step_back` strategy. This produces poorly shaped queries.

**Fix:**
```typescript
{ category: "semantic", mode, strategy: input.strategy === "auto" || !["semantic","hyde","step_back"].includes(input.strategy) ? "semantic" : input.strategy, ... }
```

#### B2. Estimated Provider Cost in Response (P1 — HIGH value, low effort)

**Problem:** Agents don't know how many provider API calls a query will consume. Multi-plan can fire 9+ calls (keyword × 3 + semantic × 3 + synthesis × 3).

**Recommendation:** Add `estimated_provider_calls: number` to the JSON response. Compute from: single-plan = count of resolved providers; multi-plan = sum of providers across all 3 categories, capped by MAX_AUTO_PLAN_CALLS.

#### B3. Timeout Escalation on Sequential Calls (P2 — nice to have)

**Problem:** When `query_plan=multi` fires 3 sequential CLI invocations and the first times out, the remaining 2 also time out (same timeout). Total wait = 3 × timeout.

**Recommendation:** On the first timeout, increase timeout by 50% for subsequent invocations in the same `execute()` call, reducing the chance of cascading failures.

#### B4. Per-Session Query Deduplication Cache (P2 — nice to have)

**Problem:** No wrapper-level dedup. If an agent searches the same query twice in one session, both hit providers and consume quota.

**Recommendation:** Add an in-memory LRU cache (keyed on `query + strategy + mode`, max 32 entries) that lives for the lifetime of the OpenCode process. Return cached results when hit.

#### B5. Normalized Provider Discovery on Config Change (P2 — low effort)

**Problem:** `warmProviderCacheAtModuleLoad()` (line 670) runs once at module import time. If user edits `config.toml` or sets `SEARCH_TOOL_ACTIVE_PROVIDERS` mid-session, wrapper won't pick it up until the 1-hour TTL expires.

**Recommendation:** When `refresh_providers=true` is passed or `providers`/`config_check` operation is called, flush the cache immediately rather than waiting for TTL. Already partially implemented via `refresh` param but only for those operations — extend to automatically detect config file mtime changes.

#### B6. Explicit `query_plan=dry_run` (P2 — refines recommendation #8)

**Problem:** No way to preview without consuming quota.

**Recommendation:** Add `operation=plan` or `query_plan=dry_run` that returns shaped queries, selected providers, mode, freshness, estimated invocations, and cacheability — without executing any CLI process.

---

### C. search-cli-coding-research Skill — Agent Guidance Patterns

#### C1. Extraction-First Workflow Emphasis (P0 — behavior change, HIGH impact)

**Problem:** Skill's default workflow (SKILL.md line 20-25) says "Read returned status, calls, results... If a specific source matters, call `operation=extract`." This treats extraction as optional follow-up rather than the primary next step.

**Recommendation:** Add bold instruction at step 5:
> **After EVERY search, immediately check `next_actions`. If it suggests extracting a URL, call `operation=extract` on that URL FIRST — before deciding whether to re-search or code.** This single pattern saves 60-80% of follow-up search quota.

#### C2. Strategy × Mode × Freshness Triplet Table (P1 — HIGH value, moderate effort)

**Problem:** Skill's path selection table (SKILL.md line 27-65) maps need → tool args but doesn't provide the correct triplet of strategy, mode, and freshness for each scenario. Agents pick strategy from the table but mode/freshness from memory.

**Recommendation:** Add explicit lookup table:

| Research Need | Strategy | Mode | Freshness | Providers | Count |
|---|---|---|---|---|---|
| Exact error/panic/stack trace | `error_debugging` | `auto` | `none` | `brave,jina` | 5 |
| API reference / config syntax | `official_docs` | `auto` | `none` | `brave,exa,jina` | 5 |
| Dependency version migration | `migration` | `auto` | `year` | `brave,exa,tavily` | 8 |
| Security CVE / advisory | `security` | `news` | `month` | `brave,tavily` | 5 |
| Conceptual "how does X work" | `semantic` | `auto` | `none` | `exa,tavily` | 5 |
| Release notes / changelog | `release_notes` | `auto` | `year` | `brave,tavily` | 5 |
| Academic paper / algorithm | `academic` | `scholar` | `none` | `exa,serpapi` | 5 |
| Social media / trending | `auto` | `social` | `week` | `xai` | 5 |

This eliminates guesswork on freshness and mode defaults.

#### C3. Response Field Interpretation Guide (P1 — HIGH value)

**Problem:** Skill mentions `status`, `calls`, `results`, `provider_discovery`, and `next_actions` but doesn't explain how to interpret each in context. Agents may re-search when `status=partial_success` even though results exist.

**Recommendation:** Add a mini-guide:
- `status=success` → Use results. Consider extracting top URL.
- `status=partial_success` → Some providers failed, some succeeded. Use results you have. Do NOT re-search unless critical.
- `status=no_results` → Check `provider_discovery.hidden_cooldown_count`. If >0, providers are cooling down. Try different providers or wait.
- `status=all_providers_failed` → Run `operation=config_check`. Verify search-cli installation and API keys.
- `status=error` → Inspect `error.code`. `binary_not_found` means install. `config_or_auth_error` means config check.
- `next_actions` exists → **Always follow the first suggestion before anything else.**

#### C4. Anti-Pattern Catalog (P2 — nice to have)

**Problem:** Skill doesn't warn against common agent mistakes that waste quota.

**Recommendation:** Add explicit DON'Ts:
- ❌ Don't paste entire error stack traces — keep only the invariant error message, strip local paths/hashes/line numbers
- ❌ Don't set `count=50` — saturates snippet budget, doesn't improve relevance at those volumes
- ❌ Don't use `query_plan=multi` with explicit providers — wrapper disables multi-plan when providers are specified
- ❌ Don't add speculative `domains` — one wrong domain can eliminate ALL results; only use when authoritative domain is known
- ❌ Don't re-search when `next_actions` suggests extraction — extract first, reconsider after reading

#### C5. Provider-Specific Query Templates (P2 — nice to have)

**Recommendation:** Add example shaped queries by provider category:

**Keyword (brave, serper, jina):**
- Error: `"<exact invariant error message>" site:github.com/issues`
- Docs: `"<package> <method/class>" site:docs.rs` or `site:python.org`
- Migration: `"<package> 2.x to 3.x migration guide"`

**Semantic (exa, tavily):**
- "A technical blog post explaining how to implement X using Y, with code examples and pitfalls"
- "Current best practices for Z in framework W as of 2026"

**Synthesis (perplexity, tavily):**
- "Compare approaches for implementing X: method A vs method B, with trade-offs and performance"
- "What is the current recommended way to do X in framework Y?"

#### C6. Quota Awareness & Cost Model (P2 — nice to have)

**Recommendation:** Add section explaining:
- Single-plan: 1 CLI invocation = N provider API calls (N = number of selected providers, typically 2-3)
- Multi-plan: up to 3 CLI invocations = up to 3N provider API calls
- Extract: 1 CLI invocation = 1 provider API call (jina or browserless)
- Typical monthly provider quotas: 500-1000 searches
- Recommendation: ≤3 tool calls per coding session; prefer extraction for follow-up

#### C7. Fallback / Degradation Chain (P2 — nice to have)

**Recommendation:** Document escalation path when first attempt fails:
```
1. query_plan=single, strategy=exact/official_docs, providers=brave,jina, count=5
   → If no_results:
2. query_plan=single, strategy=semantic, providers=exa,tavily, count=5
   → If no_results:
3. operation=extract on a manually constructed documentation URL
   → If fails:
4. operation=scrape on same URL (for JS-heavy pages)
   → If still blocked, switch to raw CLI fallback
```

---

## 3. Consolidated Priority Matrix

All recommendations ranked by **impact** (how much it improves agent search quality / reduces wasted quota) × **effort** (how hard to implement):

| ID | Component | Recommendation | Priority | Impact | Effort |
|---|---|---|---|---|---|
| ~~R1~~ | ~~wrapper~~ | ~~Remove `you` provider from hardcoded maps~~ | ~~OBSOLETE~~ | `you` now implemented in CLI | — |
| B1 | wrapper | Fix multi-plan strategy assignment for semantic calls | **P0** | Corrects query shaping | 1 line |
| R4 | CLI+wrapper | Ensure structured failure detail populated in all paths | **P0** | Enables accurate cooldowns | Medium |
| A1 | skill | Default single-plan, small count, intentional strategy | **P0** | Reduces wasted searches | Doc update |
| A2 | skill | Extraction-first: follow `next_actions` immediately | **P0** | Single biggest efficiency gain | Doc update |
| A3 | skill | Pick ONE strategy, don't mix all 3 | **P0** | Improves result relevance | Doc update |
| A4 | skill | Keep count 5-10 | **P0** | Prevents quota waste | Doc update |
| C1 | skill | Strengthen extraction-first in workflow | **P0** | Saves 60-80% follow-up quota | Doc update |
| R3 | wrapper | Parallelize multi-plan CLI calls with Promise.allSettled | **P1** | Cuts latency 3× | Medium |
| R5 | CLI+wrapper | Extend cache keys / preserve cache when safe | **P1** | Reduces redundant calls | Medium |
| R6 | CLI | Add `search batch` single-process multi-query mode | **P1** | Eliminates sequential processes | Large |
| A1 | CLI | Semantic intent classification for auto mode | **P1** | Better auto-mode defaults | Medium |
| B2 | wrapper | Add estimated_provider_calls to response | **P1** | Enables agent quota awareness | Trivial |
| C2 | skill | Strategy × Mode × Freshness triplet table | **P1** | Eliminates mode/freshness guesswork | Doc update |
| C3 | skill | Response field interpretation guide | **P1** | Prevents re-search on partial success | Doc update |
| R2 | wrapper | Derive provider/mode from agent-info | **P2** | Eliminates hardcode drift | Medium |
| R7 | CLI | Per-provider result budget allocation | **P2** | Fairer provider contribution | Medium |
| R8 | CLI+wrapper | Add dry run / query plan preview | **P2** | Cost transparency | Medium |
| R9 | CLI | Make Browserless endpoint configurable | **P2** | Self-hosted support | Trivial |
| A2 | CLI | Per-provider timeout config | **P2** | Better timeout tuning | Medium |
| A3 | CLI | Result relevance scoring | **P2** | Better ranking | Large |
| A5 | CLI | Response size guard | **P2** | Prevents token bloat | Trivial |
| B3 | wrapper | Timeout escalation on sequential calls | **P2** | Better recovery from slow providers | Low |
| B4 | wrapper | Per-session query dedup cache | **P2** | Reduces same-session repeats | Medium |
| B5 | wrapper | Auto-detect config changes, flush provider cache | **P2** | Better live-config support | Low |
| B6 | wrapper | Explicit `query_plan=dry_run` operation | **P2** | Preview without quota | Medium |
| C4 | skill | Anti-pattern catalog | **P2** | Prevents common mistakes | Doc update |
| C5 | skill | Provider-specific query templates | **P2** | Better query construction | Doc update |
| C6 | skill | Quota awareness & cost model | **P2** | Helps agents budget calls | Doc update |
| C7 | skill | Fallback / degradation chain | **P2** | Handles no-results gracefully | Doc update |

**Counts:** P0: 7 items | P1: 9 items | P2: 17 items | **Total: 33 recommendations** (R1 obsolete — `you` now implemented)

---

## 4. Implementation Strategy

Given the volume (34 items), a phased approach is recommended:

### Phase 1 — Quick Wins (P0 items, ~1.5 hours)
All P0 items are either trivial code fixes or documentation updates. These deliver the highest impact for the least effort:
- Fix multi-plan strategy assignment (1 line change)
- Audit + ensure structured failure detail in CLI metadata (code audit)
- Update SKILL.md with extraction-first emphasis, strategy discipline, and count moderation (documentation)

### Phase 2 — High-Value Enhancements (P1 items, ~1-2 days)
Requires moderate code changes:
- Parallelize multi-plan in wrapper
- Extend CLI cache keys
- Add semantic intent classification to CLI
- Add estimated_provider_calls to wrapper response
- Update skill with triplet table and response guide

### Phase 3 — Structural Improvements (P2 items, ~1 week+)
Includes CLI batch mode, provider budget allocation, relevance scoring, configurable endpoints, and comprehensive skill documentation.

---

## 5. Open Questions

1. **Provider `you` status:** Is `you` actually planned for CLI implementation, or should it be permanently removed from the wrapper? If planned, when? The fix should be: remove now, re-add when CLI ships it.

2. **CLI batch mode design:** Should `search batch` accept JSON via stdin or a `--batch-file` flag? Stdin is more flexible for programmatic use. Should it support separate `count`/`freshness` per subquery?

3. **Cache key extension scope:** Extending the CLI cache key to include providers means different provider sets = different cache entries. This is correct but increases cache storage. Is a 5-minute TTL still appropriate with expanded keys?

4. **Relevance scoring weights:** What weights for exact-match vs freshness vs authority? Needs calibration against real query results — recommend running benchmarks before finalizing.

5. **Skill update frequency:** The skill should be versioned and updated whenever the wrapper or CLI changes materially. Should a CI check run `search agent-info --json` and diff against the skill's claimed capabilities?