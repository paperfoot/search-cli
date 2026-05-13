## Program goal

Improve search quality and quota efficiency across the search-cli Rust binary, the OpenCode `search.ts` wrapper, and the `search-cli-coding-research` agent skill. Deliver higher-quality search results to LLM coding agents with fewer provider API calls per query.

## Source references

Primary planning artifact:
- `documents/search-cli-improvement-recommendations.md` — 34-item consolidated review

Design references:
- `assets/.agents/tool/opencode/search.ts` — 1315-line TypeScript wrapper
- `assets/.agents/skills/search-cli-coding-research/SKILL.md` — main skill document
- `assets/.agents/skills/search-cli-coding-research/references/refactor-notes.md` — existing independent review (13 items)
- search-cli Rust source: `src/engine.rs`, `src/cache.rs`, `src/classify.rs`, `src/types.rs`, `src/providers/`

## Context summary

The search-cli system enables LLM coding agents to search the web via 13 providers through a Rust CLI binary. The OpenCode `search.ts` wrapper adds provider discovery, cooldowns, query shaping, and result normalization. The `search-cli-coding-research` skill instructs agents how to use the tool.

An independent review identified 9 wrapper improvements and 4 agent-behavior improvements. Our additional source-level inspection found 16 more recommendations across CLI engine, wrapper planning, and skill guidance — totaling 34 items across P0/P1/P2 priorities.

## Success criteria

- [ ] All P0 recommendations implemented (multi-plan fix, CLI failure detail, skill extraction-first + single-plan defaults)
- [ ] All P1 recommendations implemented (CLI semantic classification, cache extension, parallel multi-plan, skill triplet table + response guide, estimated cost)
- [ ] P2 items captured as backlog beads for future phases
- [ ] CLI `search batch` mode designed and implemented
- [ ] Skill updated with anti-pattern catalog, provider templates, quota awareness, fallback chain
- [ ] All changes verified: `search.ts` runs without errors, `bd` beads linked correctly, skill docs consistent with wrapper behavior

## Non-goals

- Not changing the provider trait interface or adding new providers
- Not removing any existing search modes
- Not modifying the CLI's clap argument interface (extensions only)
- `you` provider is now fully implemented in CLI — no removal needed

## Child Bead plan

1. **Phase 1 — P0 Quick Wins** (epic container)
   - Fix multi-plan strategy assignment bug
   - Ensure structured failure detail in CLI metadata populated consistently
   - Update skill: extraction-first emphasis, single-plan default, intentional strategy, moderate count

2. **Phase 2 — P1 High-Value Enhancements** (epic container)
   - CLI: semantic intent classification for auto mode
   - CLI: extend cache keys to include providers/domains/freshness
   - Wrapper: parallelize multi-plan CLI calls with Promise.allSettled
   - Wrapper: add estimated_provider_calls to response
   - Skill: strategy×mode×freshness triplet table
   - Skill: response field interpretation guide

3. **Phase 3 — P2 Backlog Improvements** (epic container)
   - CLI: per-provider timeout config
   - CLI: result relevance scoring
   - CLI: per-provider result budget allocation
   - CLI: response size guard
   - CLI: configurable browserless endpoint
   - Wrapper: derive provider metadata from agent-info
   - Wrapper: timeout escalation
   - Wrapper: per-session dedup cache
   - Wrapper: auto-detect config changes
   - Wrapper: dry_run query plan
   - Skill: anti-pattern catalog
   - Skill: provider-specific query templates
   - Skill: quota awareness section
   - Skill: fallback chain documentation

4. **Skill Documentation Refresh** (standalone, after Phase 1+2 bead groups complete)
   - Full skill SKILL.md rewrite incorporating all P0/P1/P2 guidance improvements

## Dependency strategy

- Phase 1 beads can be implemented in parallel (independent changes)
- Phase 2 beads depend on Phase 1 (clean foundation)
- Phase 3 beads depend on Phase 2 (incremental)
- Skill documentation refresh bead depends on Phase 1+2 skill beads

## Approval gates

- Before Phase 2 CLI cache extension: verify cache behavior doesn't regress with existing tests
- Before Phase 3 CLI result scoring: benchmark against current ranking to ensure no quality regression
- Before Skill rewrite: review all bead notes for consistency

## Verification strategy

- Each bead includes validation commands
- Phase 1: manual CLI run + lint + existing tests
- Phase 2: run search-cli test suite + wrapper integration test
- Skill: verify agent following updated skill correctly selects single-plan, extraction-first, appropriate strategy

## Research routing

Future recommendation ideas go to: Research and Consideration Backlog bead (to be created if needed)

## Acceptance Criteria

- [ ] All 34 recommendations are captured as beads (epic + child groups)
- [ ] Root epic links to all child bead groups via parent-child
- [ ] Phase dependencies are recorded (Phase 2 beads block on Phase 1)
- [ ] `bd dep cycles` returns clean
- [ ] Recommendations document filed in `documents/`

## Closure criteria

- [ ] All Phase 1 beads complete and closed
- [ ] All Phase 2 beads complete and closed
- [ ] Phase 3 backlog beads created and linked
- [ ] Skill documentation bead complete
- [ ] `bd dep cycles` returns clean (no dependency cycles)
- [ ] `bd dolt push` successful