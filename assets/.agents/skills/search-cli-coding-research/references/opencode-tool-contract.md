# OpenCode Search Tool Contract

This skill assumes the attached `.opencode/tools/search.ts` wrapper is installed as an OpenCode custom tool named `search`.

## Preferred interface

Use the wrapper first. It adds useful guardrails over the raw CLI:

- discovers active providers from environment/config without quota-consuming probes;
- filters unavailable providers with `provider_policy=auto`;
- shapes keyword, semantic, synthesis, vertical, and extraction queries;
- normalizes/truncates JSON results for LLM consumption;
- returns `provider_discovery`, `calls`, `results`, and `next_actions`;
- supports provider cooldown logic for quota/rate-limit failures where failure detail is available.

## Core arguments

- `operation`: `search`, `extract`, `scrape`, `similar`, `providers`, `agent_info`, or `config_check`.
- `query`: search query or URL.
- `mode`: `auto`, `general`, `news`, `academic`, `people`, `deep`, `extract`, `scrape`, `similar`, `scholar`, `patents`, `images`, `places`, `social`.
- `strategy`: `exact`, `semantic`, `hyde`, `hype`, `step_back`, `official_docs`, `release_notes`, `migration`, `error_debugging`, `security`, `community`, `academic`, or `auto`.
- `query_plan`: `single`, `multi`, or `auto`. Prefer `single` for quota discipline.
- `providers`: comma-separated provider names. For this environment prefer `brave`, `browserless`, `exa`, `jina`, `tavily`.
- `provider_policy`: use `auto` unless validating setup; `strict` for explicit tests; `raw` only for wrapper debugging.
- `domains`: comma-separated hard include domain list. Use sparingly.
- `exclude_domains`: additional hard excludes.
- `freshness`: `none`, `day`, `week`, `month`, `year`, or `auto`.
- `task_context`: concise local context appended to shaped queries.
- `count`: use 5-10 by default.
- `max_snippet_chars`: raise for extraction.
- `include_raw`: use only when debugging wrapper/CLI behavior.

## Response fields to inspect

- `status`: `success`, `partial_success`, `no_results`, `all_providers_failed`, or `error`.
- `provider_discovery.configured`: active provider list used by the wrapper.
- `calls`: per-invocation shaped query, mode, requested providers, and metadata.
- `results`: deduped normalized results.
- `tool.invocations`: wrapper debug view of generated CLI args.
- `next_actions`: often suggests extraction of the best URL.

## Important wrapper behavior

- `query_plan=multi` currently creates up to three CLI invocations inside one OpenCode tool call. Use it deliberately; it is not one CLI process call.
- The wrapper almost always passes explicit providers, which prevents unconfigured-provider noise but can disable some search-cli cache paths.
- The wrapper adds a low-signal domain denylist to most searches. This is usually good for coding tasks, but it can hide relevant beginner docs or videos if those are intentionally needed.
- Direct CLI fallback is reasonable for `search agent-info --json`, `search providers --json`, or testing whether the wrapper's provider manifest drifted from CLI capabilities.

## Raw CLI fallback patterns

Use raw CLI only when the wrapper blocks what is needed or to validate setup.

```bash
search agent-info --json
search providers --json
search search -q "<query>" -m general -p brave,exa,tavily -c 8 --json
search search -q "<url>" -m extract -p jina -c 1 --json
```

Do not use raw CLI for routine coding research if the OpenCode wrapper is working.
