## Objective

Implement all P1 (high-value, moderate-effort) recommendations from the search-cli improvement review. These changes deliver significant improvements to search quality and quota efficiency with moderate implementation effort.

## Source references

- `documents/search-cli-improvement-recommendations.md` — §2, §3, priority matrix (P1 items)
- `assets/.agents/tool/opencode/search.ts` — wrapper source (lines 1215-1236 for multi-plan execution, 976-992 for next_actions)
- search-cli `src/classify.rs` — intent classification
- search-cli `src/cache.rs` — cache key construction

## Context summary

Phase 1 establishes clean defaults (single-plan, extraction-first, strategy discipline). Phase 2 extends with:
- **CLI:** Semantic intent classification so `auto` mode picks optimal strategy without caller override
- **CLI:** Extended cache keys to include providers/domains/freshness, allowing the CLI to cache even with explicit provider selection
- **Wrapper:** Parallelize multi-plan CLI calls (`Promise.allSettled` instead of sequential `for` loop)
- **Wrapper:** Add `estimated_provider_calls` to response for agent quota awareness
- **Skill:** Strategy×Mode×Freshness triplet table eliminating guesswork
- **Skill:** Response field interpretation guide teaching agents to read status correctly

## Current behavior

- CLI classify.rs: regex-only classification; many doc/API queries don't match any pattern, defaulting to `general`
- CLI cache.rs: cache skipped when ANY of providers/domains/freshness is passed — wrapper always passes providers, disabling cache entirely
- Wrapper execute(): multi-plan runs sequential `for` loop (lines 1216-1236), each call waits for previous to finish
- Wrapper: no cost estimate in response
- Skill: path table maps need→tool args but doesn't specify mode or freshness for each
- Skill: mentions `status` field but doesn't explain what each status value means or how to react

## Desired behavior

- CLI classify.rs: semantic layer (pattern matching on query intent) before regex fallback; auto mode selects better defaults
- CLI cache.rs: cache key extended to `hash(query + mode + providers + domains + freshness)`; cache used even with explicit providers
- Wrapper execute(): multi-plan calls run concurrently via `Promise.allSettled`, reducing latency from 3× sequential to ~1.5× max
- Wrapper response: includes `estimated_provider_calls: number` computed from resolved provider count × plan size
- Skill: includes explicit triplet table (strategy, mode, freshness, providers, count for 8 research patterns)
- Skill: includes mini-guide: "status=success → use results, extract top URL. status=partial_success → use what you have, don't re-search. status=no_results → check cooldowns. status=error → inspect error.code."

## Scope

In scope:
- CLI: add intent classification layer in `classify.rs` (query pattern → strategy heuristic)
- CLI: extend `CacheKey` hash in `cache.rs` to include providers, domains, freshness
- CLI: ensure `auto` mode in `engine.rs` uses new classification to select strategy
- Wrapper: replace sequential for-loop with `Promise.allSettled` for multi-plan
- Wrapper: compute and return `estimated_provider_calls` in response
- Skill: add triplet table in path selection section
- Skill: add response interpretation guide

Out of scope:
- Any Phase 3 items (relevance scoring, batch mode, timeout config, provider budgets)
- Provider trait changes

## Mandatory code/spec reading before editing

- [ ] search-cli `src/classify.rs` — full file, current regex patterns and match logic
- [ ] search-cli `src/cache.rs` — cache key construction, `is_cacheable` logic
- [ ] search-cli `src/engine.rs` — `collect_results`, auto mode speculative execution
- [ ] `assets/.agents/tool/opencode/search.ts` — lines 1215-1236 (sequential execution), lines 976-992 (next_actions), lines 842-857 (invocation construction)
- [ ] `assets/.agents/skills/search-cli-coding-research/SKILL.md` — lines 18-25 (workflow), lines 27-65 (path table)

## Implementation plan

1. **CLI classify.rs**: Add `classify_query_intent()` function with heuristic rules (URL→extract, error+matching→error_debugging, version numbers→migration, CVE/security→security, how-to/docs→official_docs). Fall back to existing regex when no heuristic matches.
2. **CLI cache.rs**: Extend `compute_cache_key()` to include providers, domains, freshness. Update `is_cacheable()` to allow caching with explicit providers when domains/freshness are absent.
3. **Wrapper execute()**: Replace `for (const invocation of plan.invocations)` with `Promise.allSettled(invocations.map(inv => runSearchCli(...)))`. Preserve result ordering and error handling.
4. **Wrapper response**: Add `estimated_provider_calls` computed from `plan.invocations.reduce((sum, inv) => sum + inv.providers.length, 0)`.
5. **Skill SKILL.md**: Insert triplet table after path selection table. Insert response guide after step 4 in default workflow.

## Acceptance Criteria

- [ ] CLI auto mode selects `error_debugging` strategy for error-like queries, `official_docs` for how-to queries, `migration` for version queries
- [ ] CLI cache returns cached results when same query+mode+providers+domains+freshness is repeated within 5min
- [ ] Wrapper multi-plan runs concurrent CLI calls (verified: 3 calls complete in ~1.5× single-call time, not 3×)
- [ ] Wrapper response includes `estimated_provider_calls` field with accurate count
- [ ] Skill includes triplet lookup table (8 rows) matching each research need to strategy/mode/freshness/providers/count
- [ ] Skill includes response interpretation guide covering all 6 status values
- [ ] No regression in existing CLI test suite
- [ ] Wrapper lint passes without new errors

## Error handling and edge cases

- Classification: ambiguous queries should default to `general` mode rather than misclassify
- Cache: when providers differ, cache must return different entries (no cross-provider cache pollution)
- Multi-plan concurrency: handle partial failures gracefully (some calls succeed, others fail); aggregate status correctly
- Cost estimation: when provider_policy=raw or providers explicitly passed, count correctly

## Boundaries

Always:
- Run existing CLI test suite after classification and cache changes
- Run wrapper lint after changes
- Verify cache behavior with manual test (same query twice, 2nd call faster)
- Create discovered beads for scope creep

Approval required:
- CLI cache key extension: verify with existing tests that cached responses remain correct
- Wrapper multi-plan parallelization: verify error aggregation still works