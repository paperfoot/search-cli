## Objective

Implement all P0 (must-fix-now) recommendations from the search-cli improvement review. These are the highest-impact, lowest-effort fixes that prevent bugs and dramatically improve agent search behavior.

## Source references

- `documents/search-cli-improvement-recommendations.md` — §2, §3, priority matrix
- `assets/.agents/tool/opencode/search.ts` — wrapper source (lines 175-208, 837, 909-930)
- `assets/.agents/skills/search-cli-coding-research/SKILL.md` — main skill document

## Context summary

The independent review (refactor-notes.md) and our additional analysis identified 7 P0 items (one previously listed `you` removal is obsolete — `you` is now fully implemented in CLI):
1. Fix multi-plan strategy assignment for semantic calls (bug: wrong strategy passed)
2. Ensure structured failure detail in CLI metadata (enables accurate cooldowns)
3. Skill: default to single-plan, small provider list
4. Skill: extraction-first workflow (follow `next_actions` immediately)
5. Skill: use intentional strategies, don't mix all 3
6. Skill: keep count moderate (5-10)
7. Skill: strengthen extraction-first in default workflow section

## Current behavior

- Wrapper: `you` provider in PROVIDERS/CAPABILITIES/CATEGORIES/ENV_KEYS but not in CLI validation → CLI errors if configured
- Wrapper: `buildInvocations` line 837 passes `input.strategy` directly to semantic call in multi-plan, even for mismatched strategies like `official_docs`
- CLI: `providers_failed_detail` exists in types.rs but may not be populated in all failure paths
- Skill: workflow says "If a specific source matters, call extract" — treats extraction as optional, not mandatory
- Skill: path selection table assigns strategies but doesn't enforce single-plan or moderate count

## Desired behavior

- Wrapper: multi-plan semantic call always uses `semantic`, `hyde`, or `step_back` strategy, never mismatched strategies
- CLI: every provider failure populates `ProviderFailureDetail` with provider, reason, code, cause
- Skill: default workflow step 5 says "AFTER every search, immediately check next_actions and extract the suggested URL FIRST"
- Skill: path table and workflow enforce `query_plan=single`, `count=5-10`, and picking ONE strategy per need

## Scope

In scope:
- Fix `buildInvocations` semantic-call strategy assignment
- Audit CLI provider failure paths to ensure `providers_failed_detail` is populated consistently
- Update SKILL.md: extraction-first emphasis, single-plan enforcement, strategy discipline, count moderation

Out of scope:
- Any Phase 2 or Phase 3 items
- Provider metadata changes (you provider is now implemented)

## Mandatory code/spec reading before editing

- [ ] `assets/.agents/tool/opencode/search.ts` — lines 175-208 (PROVIDERS maps), line 837 (multi-plan strategy), lines 909-930 (cooldown marking)
- [ ] `assets/.agents/skills/search-cli-coding-research/SKILL.md` — full file, especially lines 18-25 (default workflow) and 27-65 (path selection table)
- [ ] search-cli `src/types.rs` — `ResponseMetadata` struct, `ProviderFailureDetail` struct
- [ ] search-cli `src/engine.rs` — provider result collection and failure tracking

## Implementation plan

1. Fix `buildInvocations` line 837: change `strategy: input.strategy === "auto" ? "semantic" : input.strategy` to `strategy: input.strategy === "auto" || !["semantic","hyde","step_back"].includes(input.strategy) ? "semantic" : input.strategy`
2. Audit CLI provider implementation files to ensure `providers_failed_detail` populated in all failure paths (brave, serper, exa, jina, etc.)
3. Update SKILL.md: 
   - Step 5: bold "AFTER EVERY search, immediately check next_actions and extract first"
   - Path selection table: add `query_plan: "single"` to every row
   - Add sentence: "Pick one strategy per search. Do not mix keyword, semantic, and synthesis."
   - Add sentence: "Default count is 5. Never exceed 10 unless extracting a known URL."

## Acceptance Criteria

- [ ] Multi-plan semantic call uses semantic/hyde/step_back strategy regardless of input strategy
- [ ] CLI provider implementations audited; all populate providers_failed_detail structure on failure
- [ ] SKILL.md updated with extraction-first emphasis, single-plan defaults, strategy discipline, count moderation
- [ ] `search.ts` passes lint without new errors

## Error handling and edge cases

- Verify that removing `you` doesn't break any existing provider discovery path
- Verify multi-plan fix doesn't regress single-plan behavior
- Verify CLI failure detail population doesn't break existing JSON response format

## Boundaries

Always:
- Preserve all other provider, strategy, mode behavior unchanged
- Run existing tests where present
- Create discovered beads for scope creep

Approval required:
- None (Phase 1 is minimal risk, well-understood fixes)