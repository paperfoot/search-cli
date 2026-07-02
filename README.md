<div align="center">

# Search CLI — Web Search for AI Agents

**One binary, 13 providers, 13 modes, rank-fused results. The web search tool your AI agent is missing.**

<br />

[![Star this repo](https://img.shields.io/github/stars/paperfoot/search-cli?style=for-the-badge&logo=github&label=%E2%AD%90%20Star%20this%20repo&color=yellow)](https://github.com/paperfoot/search-cli/stargazers)
&nbsp;&nbsp;
[![Follow @longevityboris](https://img.shields.io/badge/Follow_%40longevityboris-000000?style=for-the-badge&logo=x&logoColor=white)](https://x.com/longevityboris)

<br />

[![Crates.io](https://img.shields.io/crates/v/agent-search?style=for-the-badge&logo=rust&logoColor=white&label=crates.io)](https://crates.io/crates/agent-search)
[![Downloads](https://img.shields.io/crates/d/agent-search?style=for-the-badge&logo=rust&logoColor=white)](https://crates.io/crates/agent-search)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue?style=for-the-badge)](LICENSE)
[![Build](https://img.shields.io/github/actions/workflow/status/paperfoot/search-cli/ci.yml?style=for-the-badge&logo=github)](https://github.com/paperfoot/search-cli/actions)

---

A single Rust binary that aggregates Brave, Serper, Exa, Linkup, Jina, Firecrawl, Tavily, SerpApi, Perplexity, Parallel, xAI, and more into one search interface. Built for AI agents from day one: structured JSON, semantic exit codes, self-describing `agent-info`, reciprocal rank fusion across providers, and a `usage` command that reports remaining API credits.

[Install](#install) | [How It Works](#how-it-works) | [Features](#features) | [Providers](#providers) | [Contributing](#contributing)

</div>

## Why This Exists

Every search API is good at something different. Brave has its own 35-billion page index. Serper gives you raw Google results plus Scholar, Patents, and Places. Exa does neural/semantic search. Perplexity gives AI-synthesized answers with citations. Jina reads any URL into clean markdown. Firecrawl renders JavaScript-heavy pages. xAI searches X/Twitter.

You shouldn't have to wire up each one separately, handle their different response formats, or manage rate limits. `search` does the plumbing: you pick the mode (the CLI never guesses intent), it fans out to that mode's providers in parallel and fuses the results with reciprocal rank fusion. A URL that three engines independently return outranks any single engine's top hit — the fastest provider no longer wins by default, the most-agreed-on result does.

```bash
search "CRISPR gene therapy breakthroughs"
```

That's it — a plain `search "query"` runs a general multi-provider web search and merges the results in under 2 seconds. You stay in control of routing: pick a mode with `-m` or specific providers with `-p` (run `search agent-info` for the full map). The CLI never guesses intent from your query.

## Install

**Cargo (recommended):**
```bash
cargo install agent-search
```

**Homebrew:**
```bash
brew tap paperfoot/tap
brew install search-cli
```

**One-liner (macOS / Linux):**
```bash
curl -fsSL https://raw.githubusercontent.com/paperfoot/search-cli/master/install.sh | sh
```

**From source:**
```bash
cargo install --git https://github.com/paperfoot/search-cli
```

Binary size is ~6 MB. Startup is ~2 ms. Memory is ~5 MB. No Python, no Node, no Docker.

## Quick Start

```bash
# Set your API keys (any combination works -- even just one)
search config set keys.brave YOUR_BRAVE_KEY
search config set keys.serper YOUR_SERPER_KEY
search config set keys.exa YOUR_EXA_KEY

# Or use environment variables
export SEARCH_KEYS_BRAVE=YOUR_KEY
export SEARCH_KEYS_EXA=YOUR_KEY

# Search
search "your query here"
```

## How It Works

```
                          ┌─────────────┐
                          │ Query + -m  │  you pick the mode —
                          └──────┬──────┘  no intent guessing
                                 │
                    ┌────────────┼────────────┐
                    ▼            ▼            ▼
              ┌──────────┐ ┌──────────┐ ┌──────────┐
              │  Brave   │ │  Serper  │ │   Exa    │  parallel fan-out
              └────┬─────┘ └────┬─────┘ └────┬─────┘  via tokio::JoinSet
                   │            │            │
                   └────────────┼────────────┘
                                │
                         ┌──────▼──────┐
                         │ Rank fusion │  dedup + reciprocal rank
                         │  (RRF k=60) │  fusion across providers
                         └──────┬──────┘
                                │
                    ┌───────────┴───────────┐
                    ▼                       ▼
             ┌────────────┐         ┌────────────┐
             │    JSON    │         │   Table    │
             │  (piped)   │         │ (terminal) │
             └────────────┘         └────────────┘
```

1. **Parse** -- Clap parses your query, mode, provider filter, and output preferences
2. **Route** -- Your mode determines which providers to query (or you override with `-p`)
3. **Fan out** -- `tokio::JoinSet` fires all providers in parallel with per-provider timeouts
4. **Collect** -- Once enough unique results arrive, stragglers get a 1.5s grace window, then are cancelled (reported in `metadata.providers_cancelled`; `deep` mode always waits for everyone)
5. **Fuse** -- Reciprocal rank fusion: URLs returned by multiple providers rank first (`extra.also_found_by` records consensus); ordering is deterministic, not fastest-provider-first
6. **Separate answers** -- AI-synthesized answers (Perplexity/Tavily) land in `answers[]`, so every `results[].url` is a fetchable web URL
7. **Render** -- JSON envelope when piped, colored terminal table when interactive

## Features

### 13 Search Modes

Pick a mode explicitly with `-m` (default is `general`). The `-q` column
matters: `extract`/`scrape`/`similar` take a URL and reject text queries
(exit 3). This table mirrors `search agent-info`, which is generated from the
same routing registry the engine uses.

| Mode | Use when | `-q` is | Providers used |
|------|----------|---------|----------------|
| `general` | Any web lookup not covered below (default) | query | Parallel + Brave + Serper + Exa + Jina + Linkup + Tavily + Perplexity |
| `news` | Current events; add `-f day`/`-f week` | query | Parallel + Brave + Serper + Linkup + Tavily + Perplexity (news endpoints) |
| `academic` | Papers/studies by topic (semantic + web) | query | Exa + Serper + Tavily + Perplexity |
| `scholar` | Google Scholar records: citations, PDFs | query | Serper + SerpApi |
| `deep` | Max coverage; waits for all providers — use `-c 30` | query | Parallel + Brave (web + LLM Context) + Serper + Exa + Linkup + Tavily + Perplexity + xAI |
| `people` | A person, their role, LinkedIn profile | query | Exa |
| `social` | What's being said on X/Twitter | query | xAI (Grok) |
| `patents` | Prior art, patent families | query | Serper |
| `images` | Image search (check `image_url` fields) | query | Serper |
| `places` | Local businesses, maps | query | Serper |
| `extract` | Read one page as markdown, incl. JS/anti-bot | **URL** | Stealth -> Jina -> Firecrawl -> Browserless |
| `scrape` | Alias of `extract` (identical) | **URL** | Stealth -> Jina -> Firecrawl -> Browserless |
| `similar` | "More like this page" | **URL** | Exa |

### Rank Fusion, Not Fastest-Provider-First

Most meta-search tools return results in whatever order providers answer, so result #1 is the fastest API's opinion. `search` scores every URL across all providers (RRF, k=60): consensus ranks first, ordering is deterministic, and each result carries `extra.also_found_by` so you can see which engines agreed. The envelope tells you exactly what happened — who contributed (`provider_results`), who was cancelled (`providers_cancelled`), which filters a provider ignored (`warnings`), and whether you got a cached replay (`cached`).

### Agent-First Design

Built for Claude Code, Codex CLI, Gemini CLI, [OpenClaw](https://github.com/openclaw), and any AI agent that can shell out to a command.

```bash
# Discover capabilities programmatically
search agent-info

# Structured JSON with metadata
search "query" --json
# {
#   "status": "success",
#   "query": "...",
#   "mode": "general",
#   "results": [...],                              // rank-fused, deduped
#   "answers": [{"provider": "perplexity_sonar", "text": "..."}],
#   "metadata": {
#     "elapsed_ms": 1542,
#     "result_count": 10,
#     "providers_queried": ["brave", "serper", "exa", "jina"],
#     "provider_results": {"brave": 10, "serper": 10},  // who contributed
#     "providers_cancelled": ["jina"],             // cut off after enough results
#     "warnings": []                               // e.g. filters a provider ignored
#   }
# }

# Check remaining credits (where the provider API exposes them)
search usage --json
```

**Auto-JSON:** Output is automatically JSON when piped to another program. Human-readable tables when you're in a terminal.

**Semantic exit codes:**

| Code | Meaning | Agent action |
|------|---------|-------------|
| 0 | Success | Process results |
| 1 | Transient error (API, network) | Retry might help |
| 2 | Config or auth error | Fix setup / set API key |
| 3 | Bad input | Fix arguments |
| 4 | Rate limited | Back off and retry |

### Usage Examples

```bash
# Default general web search (no mode = general; no intent guessing)
search "quantum computing advances"
search search -q "who is the CEO of Anthropic" -m people
search search -q "CRISPR research papers" -m academic

# Force a specific mode
search search -q "transformer architectures" -m academic
search search -q "Sam Altman" -m people
search search -q "AI startups 2026" -m news
search search -q "BRCA1 gene patent" -m patents

# Search X (Twitter) only
search --x "AI agents"

# URL modes take a URL, not a query
search search -q https://example.com/article -m extract
search search -q https://stripe.com -m similar

# Pick specific providers
search search -q "machine learning" -p exa
search search -q "rust programming" -p brave,serper

# Control output
search "query" --json | jq '.results[].url'
search "query" -c 20                   # 20 results
search "query" 2>/dev/null             # suppress diagnostics
```

## Providers

| Provider | What it does | Best for |
|----------|-------------|----------|
| **[Parallel](https://parallel.ai/)** | Agent-native search: objective in, LLM-ready ranked excerpts out | Grounding content for agents, deterministic per-query cost |
| **[Brave](https://brave.com/search/api/)** | Independent index (not resold Google/Bing) + LLM Context API | Independent ranking signal, news, RAG grounding |
| **[Serper](https://serper.dev/)** | Cheapest raw Google SERP + specialist endpoints | Actual Google rankings; scholar, patents, images, places |
| **[Exa](https://exa.ai/)** | Neural/semantic search, category filters | Research papers, people search, similar sites |
| **[Jina](https://jina.ai/)** | Fast URL-to-markdown, 500 RPM free tier | Reading article content, quick extraction |
| **[Linkup](https://www.linkup.so/)** | High-accuracy agent search (leads the SimpleQA benchmark) | Factual lookups where accuracy matters most |
| **[Firecrawl](https://firecrawl.dev/)** | JavaScript rendering, structured extraction | Dynamic pages, SPAs, data extraction |
| **[Tavily](https://tavily.com/)** | General + deep search, research-focused | Broad coverage, research queries |
| **[SerpApi](https://serpapi.com/)** | Many engines: Google, Bing, YouTube, Baidu | Multi-engine coverage; only provider with a real balance API |
| **[Perplexity](https://perplexity.ai/)** | LLM-synthesized answer with citations (Sonar) | When you want an answer with sources, not raw pages |
| **Browserless** | Cloud browser for Cloudflare/JS-heavy pages | Anti-bot bypass, pages that need a real browser |
| **Stealth** | Built-in anti-bot scraper | Protected pages, no API key needed |
| **[xAI](https://x.ai/)** | Only provider with native real-time X/Twitter search (Grok) | Live social signal, trending topics, account activity |

### Checking Credits

```bash
search usage --json
```

Reports remaining credits/quota for every provider whose API exposes it:
**SerpApi** (`account.json`), **Firecrawl** (credit-usage endpoint),
**Tavily** (usage endpoint), **Linkup** (credits balance endpoint), **xAI**
(management API — set `XAI_MANAGEMENT_API_KEY` + `XAI_TEAM_ID`), and **Brave**
(rate-limit headers; the check consumes one metered request). The rest are dashboard-only and
reported as `supported: false`. Purely informational: the
CLI never disables or deprioritizes a provider because its balance is low —
if a provider is out of credits, the failed call shows up as a
`billing_quota` entry in `metadata.provider_failures` and you decide what to
top up.

## Configuration

Config file lives at `~/.config/search/config.toml` (Linux) or `~/Library/Application Support/search/config.toml` (macOS).

```bash
search config show       # View current config (keys masked)
search config check      # Health check all providers
search config set K V    # Set a value
```

Environment variables override the config file, so a freshly-exported or
CI-injected key always wins over stale local config. Two accepted forms:

```bash
# Standard per-provider variables (recommended):
export BRAVE_API_KEY=your-key
export SERPER_API_KEY=your-key
export EXA_API_KEY=your-key
# ...also PERPLEXITY_API_KEY, JINA_API_KEY, LINKUP_API_KEY, FIRECRAWL_API_KEY,
#        TAVILY_API_KEY, SERPAPI_API_KEY, BROWSERLESS_API_KEY, XAI_API_KEY,
#        PARALLEL_API_KEY

# Or the SEARCH_KEYS_ prefixed form:
export SEARCH_KEYS_BRAVE=your-key
```

Precedence (highest first): `<PROVIDER>_API_KEY` env → `SEARCH_KEYS_*` env → config file.

## Updating

```bash
search update             # Self-update from GitHub releases
search update --check     # Check without installing
```

## Building from Source

```bash
git clone https://github.com/paperfoot/search-cli
cd search-cli
cargo build --release
# Binary at target/release/search
```

## Contributing

Contributions are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## License

[MIT](LICENSE)

---

<div align="center">

Built by [Boris Djordjevic](https://github.com/longevityboris) at [Paperfoot AI](https://paperfoot.com)

<br />

**If this is useful to you:**

[![Star this repo](https://img.shields.io/github/stars/paperfoot/search-cli?style=for-the-badge&logo=github&label=%E2%AD%90%20Star%20this%20repo&color=yellow)](https://github.com/paperfoot/search-cli/stargazers)
&nbsp;&nbsp;
[![Follow @longevityboris](https://img.shields.io/badge/Follow_%40longevityboris-000000?style=for-the-badge&logo=x&logoColor=white)](https://x.com/longevityboris)

</div>
