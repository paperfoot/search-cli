## Objective

Implement all P2 (nice-to-have) recommendations from the search-cli improvement review. These are lower-priority but still valuable improvements — structural enhancements, better ranking, provider configurability, and comprehensive skill documentation.

## Source references

- `documents/search-cli-improvement-recommendations.md` — §2, §3, priority matrix (all P2 items)
- search-cli `src/providers/` — browserless provider endpoint, all provider implementations
- search-cli `src/types.rs` — SearchResult struct
- `assets/.agents/tool/opencode/search.ts` — wrapper source (provider discovery, cooldowns, invocation building)

## Context summary

Phase 2 completed the high-value structural work. Phase 3 captures remaining improvements that require more design or implementation effort:
- **CLI:** Per-provider timeout config, result relevance scoring, per-provider budget allocation, response size guard, configurable browserless endpoint
- **Wrapper:** Dynamic provider metadata from agent-info, timeout escalation, per-session dedup cache, auto-detect config changes, dry_run query plan
- **Skill:** Anti-pattern catalog, provider-specific query templates, quota awareness section, fallback chain documentation

## Scope

In scope:
- CLI config: `[timeouts]` section per provider, browserless endpoint config
- CLI engine: relevance scoring (exact match, freshness, authority, agreement bonuses)
- CLI engine: per-provider result budget allocation
- CLI output: `--max-response-bytes` flag for size guarding
- Wrapper: read `agent-info --json` to derive provider metadata dynamically
- Wrapper: timeout escalation (increase timeout 50% per subsequent call after first timeout)
- Wrapper: in-memory LRU cache for same-session query dedup
- Wrapper: auto-detect config file mtime change, flush provider cache
- Wrapper: `query_plan=dry_run` operation mode
- Skill: DON'Ts catalog, provider query templates, quota cost model, fallback chain

Out of scope:
- CLI batch mode (`search batch`) — this is P1 (in Phase 2) and complex enough for its own bead
- New provider implementations
- Changing the CLI argument interface (extensions only)

## Mandatory code/spec reading before editing

- [ ] search-cli `src/config.rs` — config file loading, ApiKeys struct
- [ ] search-cli `src/types.rs` — SearchResult, ResponseMetadata, SearchOpts
- [ ] search-cli `src/engine.rs` — result merging, deduplication, truncation
- [ ] search-cli `src/providers/browserless.rs` — endpoint hardcoding
- [ ] search-cli `src/providers/mod.rs` — Provider trait, timeout handling
- [ ] `assets/.agents/tool/opencode/search.ts` — lines 304-306 (provider cache), 646-662 (discoverProviders), 1124-1314 (execute)
- [ ] `assets/.agents/skills/search-cli-coding-research/SKILL.md` — full file

## Implementation plan

### CLI changes
1. **Per-provider timeout**: Add `[timeouts]` to config loading, populate `SearchOpts.timeout_ms` per provider
2. **Relevance scoring**: Add `relevance: f32` to SearchResult, score in engine result collection
3. **Provider budgets**: Add `--per-provider-count` flag or config, allocate slots per provider before global dedupe
4. **Response guard**: Add `--max-response-bytes` flag, truncate snippet fields when total exceeds
5. **Browserless endpoint**: Read `BROWSERLESS_ENDPOINT` env var or config key

### Wrapper changes
6. **Dynamic metadata**: Call `agent-info --json` on provider discovery refresh, merge with hardcoded categories, reject providers not in CLI
7. **Timeout escalation**: Track consecutive timeouts in execute(), increase timeout by 50% per subsequent call
8. **Dedup cache**: Add Map<string, {results, timestamp}> keyed on query+strategy+mode hash, check before CLI calls
9. **Config change detection**: Track config file mtime, flush provider cache when changed
10. **Dry run**: Add `query_plan=dry_run` handling in buildInvocations — return shaped invocations without executing

### Skill changes
11. **Anti-patterns**: Add DON'Ts section (don't paste full stack traces, don't count=50, don't multi+explicit providers, don't speculative domains, don't re-search before extract)
12. **Provider templates**: Add shaped query examples for keyword/semantic/synthesis/extract categories
13. **Quota model**: Add section explaining cost per call type, recommended daily budget
14. **Fallback chain**: Document escalation from exact→semantic→extract→scrape→raw CLI

## Acceptance Criteria

- [ ] CLI `[timeouts]` config section works, each provider gets configured timeout
- [ ] Relevance scores appear in SearchResult and are used for ranking
- [ ] Provider budget allocation prevents fastest-provider monopoly
- [ ] `--max-response-bytes` flag works, responses truncated without breaking JSON
- [ ] Browserless endpoint reads from config/env
- [ ] Wrapper reads `agent-info` for provider metadata, no hardcoded `you` references
- [ ] Timeout escalation reduces full-failure rate on slow providers
- [ ] Same-session dedup cache prevents re-searching identical queries
- [ ] Config change auto-detection flushes provider cache without manual refresh
- [ ] `query_plan=dry_run` returns plan without consuming quota
- [ ] Skill includes anti-pattern catalog, provider templates, quota section, fallback chain
- [ ] No regression in existing tests

## Error handling and edge cases

- Timeout config: missing provider → use default; zero timeout → reject (error)
- Relevance scoring: no match data → `None` score (not zero); handle gracefully in ranking
- Dry run: must still validate providers, domains, modes — just skip execution
- Dedup cache: must respect cooldowns (cooldowned providers not cached); must clear on config change
- Config change detection: handle race between mtime read and cache flush

## Boundaries

Always:
- Run existing CLI test suite after each CLI change
- Run wrapper lint after wrapper changes
- Verify ranking doesn't regress with manual comparison before/after
- Create discovered beads for scope creep

Approval required:
- Relevance scoring weights need calibration/testing before finalizing
- Provider budget allocation needs design review (budget per provider vs global cap)
- Dynamic metadata from agent-info needs compatibility testing with current CLI output