## Objective

Rewrite the `search-cli-coding-research` SKILL.md to incorporate all Phase 1 and Phase 2 skill improvements, plus Phase 3 documentation enhancements. The resulting skill should produce agents that consistently use single-plan, extraction-first workflows with correct strategy/mode/freshness triplets.

## Source references

- `documents/search-cli-improvement-recommendations.md` — all skill recommendations (C1-C7)
- `assets/.agents/skills/search-cli-coding-research/SKILL.md` — current skill (to be replaced)
- `assets/.agents/skills/search-cli-coding-research/references/query-playbook.md` — existing query patterns
- `assets/.agents/skills/search-cli-coding-research/references/opencode-tool-contract.md` — wrapper interface

## Context summary

The current skill is functional but missing critical guidance:
- No extraction-first emphasis in workflow (treats extraction as optional follow-up)
- No strategy×mode×freshness triplet table (agents guess mode/freshness)
- No response field interpretation guide (agents re-search on partial_success)
- No anti-patterns catalog
- No provider-specific query templates
- No quota awareness section
- No fallback chain

Phase 1 adds: extraction-first, single-plan enforcement, strategy discipline, count moderation.
Phase 2 adds: triplet table, response interpretation guide.
Phase 3 adds: anti-patterns, query templates, quota model, fallback chain.

## Desired behavior

Agents reading the updated skill will:
1. Default to `query_plan=single` with 2-3 providers and count=5-10
2. After EVERY search, check `next_actions` and extract the best URL FIRST before any re-search
3. Select ONE strategy matching their research need (not mix keyword+semantic+synthesis)
4. Use the correct strategy/mode/freshness triplet from the lookup table
5. Interpret response status correctly (not re-search on partial_success)
6. Avoid common quota-wasting mistakes (pasting full stacks, count=50, speculative domains)
7. Understand the cost model (single-plan ≈ 2-3 calls, multi-plan ≈ up to 9 calls)
8. Follow the fallback chain when first search produces no results

## Scope

In scope:
- Full rewrite of SKILL.md preserving existing structure (frontmatter, operation principle, path table, workflow) but adding all new sections
- All Phase 1+2+3 skill recommendations (extraction-first, triplet table, response guide, anti-patterns, templates, quota, fallback)

Out of scope:
- Changes to query-playbook.md or opencode-tool-contract.md (those are reference docs, updated separately)
- Changing the wrapper or CLI (handled by Phase 1/2/3 implementation beads)

## Mandatory code/spec reading before editing

- [ ] Current SKILL.md (full file)
- [ ] query-playbook.md (reference patterns)
- [ ] opencode-tool-contract.md (wrapper interface)
- [ ] All Phase 1/2/3 bead notes for consistency

## Implementation plan

1. Read current SKILL.md in full
2. Draft new SKILL.md adding:
   - Extraction-first bold instruction in step 5
   - Strategy×Mode×Freshness triplet table (8 rows)
   - Response field interpretation guide (5 status values + next_actions)
   - Anti-pattern catalog (5 DON'Ts)
   - Provider-specific query templates (4 categories)
   - Quota awareness section
   - Fallback chain (4-step escalation)
3. Replace existing file
4. Verify agent following new skill correctly defaults to single-plan + extraction-first

## Acceptance Criteria

- [ ] New SKILL.md includes extraction-first emphasis in bullet 5 of default workflow
- [ ] New SKILL.md includes triplet lookup table with 8 rows (strategy, mode, freshness, providers, count)
- [ ] New SKILL.md includes response interpretation guide covering all status values
- [ ] New SKILL.md includes anti-pattern catalog with 5+ common mistakes
- [ ] New SKILL.md includes provider query templates for keyword/semantic/synthesis/extract
- [ ] New SKILL.md includes quota cost model section
- [ ] New SKILL.md includes fallback chain
- [ ] New SKILL.md preserves existing path selection table structure
- [ ] Provider references correct (you provider included since now implemented in CLI)
- [ ] New SKILL.md references updated wrapper behavior (estimated_provider_calls, parallel multi-plan)

## Error handling and edge cases

- Ensure new sections don't conflict with existing query-playbook.md patterns
- Ensure triplet table provider recommendations match actual provider availability
- Verify skill frontmatter `description` field still accurate after rewrite

## Boundaries

Always:
- Preserve existing skill structure and operating principle
- Reference wrapper contract doc for authoritative field names
- Create discovered beads for out-of-scope improvements found during rewrite

Approval required:
- Full skill rewrite should be reviewed against wrapper behavior to ensure consistency