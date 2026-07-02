# Changelog

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