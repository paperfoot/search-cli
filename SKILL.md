---
name: search
description: >
  Multi-provider search CLI with 13 explicit modes (you pick the mode/providers;
  no intent guessing). Results are rank-fused across providers. Run
  `search agent-info` for full capabilities and exit codes.
---

## search

Agent-friendly multi-provider search CLI. You choose the mode — the CLI never
guesses intent. Run `search agent-info` for the machine-readable manifest.

## Mode selection (the decision that matters)

| Mode | Use when | `-q` is |
|------|----------|---------|
| `general` (default) | Any web lookup not covered below | query |
| `news` | Current events; add `-f day` or `-f week` | query |
| `academic` | Papers/studies by topic (semantic + web) | query |
| `scholar` | Google Scholar records: citations, versions, PDFs | query |
| `deep` | Max coverage incl. X + grounding; waits for all providers — use `-c 30` | query |
| `people` | A person, their role, LinkedIn profile | query |
| `social` | What's being said on X/Twitter (summary + cited posts) | query |
| `images` / `places` / `patents` | Google Images / Maps / Patents | query |
| `extract` (alias `scrape`) | Read one page as markdown, incl. JS/anti-bot pages | **URL** |
| `similar` | "More like this page" (competitors, related coverage) | **URL** |

Quick examples:
- `search "rust error handling"` — general web search (shorthand)
- `search search -q "CRISPR delivery" -m academic --json`
- `search search -q https://example.com/post -m extract` — URL modes reject text queries (exit 3)
- `search --x "AI agents"` — X/Twitter only
- `search verify alice@stripe.com --json` — SMTP email verification
- `search usage --json` — remaining credits per provider (where the API exposes it)

## Reading the envelope

- Results are deduped and rank-fused: URLs multiple providers agree on rank
  first (`extra.also_found_by` shows consensus). Ordering is deterministic.
- `answers[]` carries AI-synthesized answers (Perplexity/Tavily) separately —
  every `results[].url` is a fetchable web URL.
- `metadata.provider_results` shows who contributed what;
  `providers_cancelled` were cut off after enough results arrived (never in
  `deep` mode); `warnings` flags filters a provider silently ignores;
  `cached: true` marks a replay from the 5-minute local cache.
- Filters: `-f day|week|month|year`, `-d domain.com`, `--exclude-domain`. In
  `social` mode `-d`/`--exclude-domain` filter X *handles*. Check
  `metadata.warnings` — not every provider applies every filter.
- Exit codes: 0 ok · 1 transient (retry) · 2 config/auth · 3 bad input · 4 rate-limited.

## Not suited for (use these instead)

- **GitHub repos/code/issues/PRs** → use `gh` CLI (GitHub's own search API):
  - `gh search repos "query" --language=rust --sort=stars --json fullName,description,stargazersCount,url`
  - `gh search code "query" --language=go --json path,repository`
  - `gh search issues "query" --state=open --json title,url,state`
