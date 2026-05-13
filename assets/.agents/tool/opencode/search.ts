/// <reference path="../env.d.ts" />
/**
 * .opencode/tools/search.ts
 *
 * OpenCode custom tool for the `search` binary from agent-search/search-cli.
 * Place this file at either:
 *   - <repo>/.opencode/tools/search.ts
 *   - ~/.config/opencode/tools/search.ts
 *
 * Design goals:
 *   - Discover which search-cli providers are actually configured before use.
 *   - Avoid advertising or selecting providers that are unavailable in the user's environment.
 *   - Shape separate queries for keyword, semantic/synthesis, vertical, and URL-extraction workflows.
 *   - Preserve search-cli's provider/mode capabilities while adding coding-agent guardrails.
 *   - Prefer JSON output and normalize/truncate results for LLM consumption.
 *
 * search-cli source of truth, branch fix/rquest-to-wreq-migration:
 *   - binary: search
 *   - crate: agent-search
 *   - modes: auto, general, news, academic, people, deep, extract, scrape,
 *            similar, scholar, patents, images, places, social
 *   - useful commands: search, providers, agent-info, config check
 * export SEARCH_TOOL_ACTIVE_PROVIDERS=brave,exa,jina,tavily
 * export SEARCH_TOOL_DISABLED_PROVIDERS=browserless,xai
 */

import { tool } from "@opencode-ai/plugin"
import { execFile } from "node:child_process"
import { existsSync, readFileSync } from "node:fs"
import { join } from "node:path"
import { homedir } from "node:os"

const DEFAULT_TIMEOUT_MS = 90_000
const MAX_TIMEOUT_MS = 180_000
const DEFAULT_MAX_SNIPPET_CHARS = 2_000
const EXTRACT_MAX_SNIPPET_CHARS = 12_000
const PROVIDER_CACHE_TTL_MS = 60 * 60_000
const PROVIDER_COOLDOWN_MS = 24 * 60 * 60_000
const MAX_AUTO_PLAN_CALLS = 3

const SEARCH_MODES = [
  "auto",
  "general",
  "news",
  "academic",
  "people",
  "deep",
  "extract",
  "scrape",
  "similar",
  "scholar",
  "patents",
  "images",
  "places",
  "social",
] as const

type SearchMode = (typeof SEARCH_MODES)[number]

type SearchOperation = "search" | "extract" | "scrape" | "similar" | "providers" | "agent_info" | "config_check"
type Freshness = "auto" | "none" | "day" | "week" | "month" | "year"
type QueryStrategy =
  | "auto"
  | "exact"
  | "semantic"
  | "hyde"
  | "hype"
  | "step_back"
  | "official_docs"
  | "release_notes"
  | "migration"
  | "error_debugging"
  | "security"
  | "community"
  | "academic"

type QueryPlan = "auto" | "single" | "multi"
type ProviderPolicy = "auto" | "strict" | "raw"
type ProviderCategory = "keyword" | "semantic" | "synthesis" | "extract" | "vertical" | "social" | "local_scrape"

type SearchArgs = {
  operation?: SearchOperation
  query?: string
  mode?: SearchMode
  count?: number
  providers?: string
  domains?: string
  exclude_domains?: string
  freshness?: Freshness
  strategy?: QueryStrategy
  query_plan?: QueryPlan
  provider_policy?: ProviderPolicy
  refresh_providers?: boolean
  task_context?: string
  max_snippet_chars?: number
  timeout_ms?: number
  include_raw?: boolean
}

type ExecError = Error & {
  code?: string | number
  status?: number | null
  signal?: NodeJS.Signals | string | null
  killed?: boolean
  stdout?: string
  stderr?: string
}

type ProviderStatus = {
  name: string
  configured: boolean
  capabilities: string[]
  categories: ProviderCategory[]
  env_keys?: string[]
}

type ProviderCooldown = {
  provider: string
  expiresAt: number
  reason: string
}

type ProviderDiscovery = {
  status: "success" | "error"
  discovery_method: "env_and_config_file" | "user_override" | "error"
  config_path?: string
  providers: ProviderStatus[]
  configured: string[]
  by_category: Record<ProviderCategory, string[]>
  cache_age_ms?: number
  hidden_unconfigured_count: number
  hidden_cooldown_count: number
  error?: string
}

type Invocation = {
  label: string
  provider_category?: ProviderCategory
  providers?: string[]
  mode: SearchMode | "command"
  shaped_query?: string
  binaryArgs: string[]
  warnings: string[]
}

const PROVIDERS = [
  "parallel",
  "brave",
  "serper",
  "exa",
  "jina",
  "firecrawl",
  "tavily",
  "serpapi",
  "perplexity",
  "browserless",
  "stealth",
  "xai",
  "you",
] as const

const PROVIDER_CAPABILITIES: Record<string, string[]> = {
  parallel: ["general", "news", "deep"],
  brave: ["general", "news", "deep"],
  serper: ["general", "news", "scholar", "patents", "images", "places"],
  exa: ["general", "academic", "people", "similar", "deep"],
  jina: ["general", "extract"],
  firecrawl: ["general", "scrape", "extract"],
  tavily: ["general", "news", "academic", "deep"],
  serpapi: ["general", "news", "scholar", "images"],
  perplexity: ["general", "news", "academic", "deep"],
  browserless: ["scrape", "extract"],
  stealth: ["scrape", "extract"],
  xai: ["social"],
  you: ["general", "news", "deep"],
}

const PROVIDER_ENV_KEYS: Record<string, string[]> = {
  parallel: ["PARALLEL_API_KEY", "SEARCH_KEYS_PARALLEL"],
  brave: ["BRAVE_API_KEY", "SEARCH_KEYS_BRAVE"],
  serper: ["SERPER_API_KEY", "SEARCH_KEYS_SERPER"],
  exa: ["EXA_API_KEY", "SEARCH_KEYS_EXA"],
  jina: ["JINA_API_KEY", "SEARCH_KEYS_JINA"],
  firecrawl: ["FIRECRAWL_API_KEY", "SEARCH_KEYS_FIRECRAWL"],
  tavily: ["TAVILY_API_KEY", "SEARCH_KEYS_TAVILY"],
  serpapi: ["SERPAPI_API_KEY", "SEARCH_KEYS_SERPAPI"],
  perplexity: ["PERPLEXITY_API_KEY", "SEARCH_KEYS_PERPLEXITY"],
  browserless: ["BROWSERLESS_API_KEY", "SEARCH_KEYS_BROWSERLESS"],
  stealth: [],
  xai: ["XAI_API_KEY", "SEARCH_KEYS_XAI"],
  you: ["YOU_API_KEY", "SEARCH_KEYS_YOU"],
}

const PROVIDER_CATEGORIES: Record<string, ProviderCategory[]> = {
  parallel: ["semantic", "synthesis"],
  brave: ["keyword"],
  serper: ["keyword", "vertical"],
  exa: ["semantic", "vertical"],
  jina: ["keyword", "extract"],
  firecrawl: ["semantic", "extract"],
  tavily: ["semantic", "synthesis"],
  serpapi: ["keyword", "vertical"],
  perplexity: ["synthesis", "semantic"],
  browserless: ["extract", "local_scrape"],
  stealth: ["extract", "local_scrape"],
  xai: ["social", "synthesis"],
  you: ["keyword", "synthesis"],
}

const CATEGORY_ORDER: ProviderCategory[] = ["keyword", "semantic", "synthesis", "vertical", "extract", "social", "local_scrape"]

const LOW_SIGNAL_EXCLUDE_DOMAINS = [
  "ebay.com",
  "amazon.com",
  "aliexpress.com",
  "etsy.com",
  "pinterest.com",
  "facebook.com",
  "instagram.com",
  "tiktok.com",
  "youtube.com",
  "w3schools.com",
  "geeksforgeeks.org",
  "tutorialspoint.com",
  "javatpoint.com",
  "studytonight.com",
  "guru99.com",
  "simplilearn.com",
  "quora.com",
]

const MODE_GUIDANCE = `
MODE SELECTION:
- auto: default for ordinary coding research when the right provider is unclear.
- general: broad web and docs lookup.
- deep: hard debugging, architecture decisions, feature design research, API ambiguity, multi-provider evidence.
- news: recent releases, changelogs, breaking changes, CVEs, outages.
- academic/scholar: papers, benchmarks, algorithms, formal methods.
- people: people/company profile lookup through Exa.
- extract: read a known URL into LLM-friendly text. Query must be a URL.
- scrape: read JS-heavy or protected pages. Query must be a URL.
- similar: find pages similar to a known URL. Query must be a URL.
- patents/images/places/social: use only when that vertical is explicitly needed.
`.trim()

const PROVIDER_GUIDANCE = `
PROVIDER DISCOVERY:
- This tool reads local environment variables and the search-cli config file to discover active providers.
- It does not probe provider APIs for availability because probes can consume quota.
- API-key presence means available, except providers placed into session cooldown after quota/rate-limit failures.
- provider_policy=auto filters provider selection to active providers.
- provider_policy=strict fails when requested providers are inactive.
- provider_policy=raw bypasses filtering and lets search-cli fail or skip providers.
- Use operation=providers to inspect only the active providers and category mapping.

PROVIDER-SPECIFIC QUERY SHAPING:
- keyword providers: brave, serper, serpapi, you. Use exact symbols, quoted errors, API names, package names, site:, -site:, OR, and recency filters.
- semantic providers: exa, tavily, parallel. Use natural-language descriptions, HyDE-style hypothetical-doc queries, and conceptual phrasing.
- synthesis providers: perplexity, tavily, parallel, you. Ask full questions and request comparison, constraints, citations, or current best practice.
- vertical providers: serper, serpapi, exa. Use scholar, patents, images, places, people, or similar when the task is explicitly vertical.
- extraction providers: stealth, jina, firecrawl, browserless. Use only after you have a URL or when operation=extract/scrape/similar.
- social provider: xai. Use for current X/Twitter developer reports, breakage chatter, maintainer statements, or launch sentiment.
`.trim()

const QUERY_STRATEGY_GUIDANCE = `
QUERY STRATEGIES:
- exact: quote exact error messages, symbols, types, filenames, config keys, or panic strings.
- semantic: describe desired behavior/API concept in natural language. Best for Exa/Tavily/Parallel.
- hyde: write the query like a hypothetical relevant answer/document would read. Best for semantic retrieval.
- hype: search for likely questions/prompts a developer would ask about the issue.
- step_back: search the underlying concept before the specific bug or implementation.
- official_docs: bias toward official documentation, API reference, changelog, migration guide, release notes.
- release_notes: search recent changelogs, deprecations, breaking changes, upgrade guides.
- migration: search before/after API differences, compatibility notes, examples, and edge cases.
- error_debugging: exact error first; then package/framework/version; then known issue/workaround.
- security: search CVE/advisory/release/mitigation terms with freshness week/month/year.
- community: search discussion, workaround, GitHub issue, Stack Overflow, Reddit/HN only after official docs.
`.trim()

const DESCRIPTION = `
Search the internet using the local search-cli binary (agent-search). This is for coding agents that need current external information before editing code: official docs, SDK APIs, package migrations, exact errors, release notes, CVEs, changelogs, research papers, URL extraction, or current developer reports.

Do not use this tool for local repository search. Use read/grep/glob/bash for local files. Do not use this tool for GitHub code/issues/PRs when the GitHub CLI or an MCP GitHub tool is available; GitHub-native APIs are better for repo metadata.

${MODE_GUIDANCE}

${PROVIDER_GUIDANCE}

${QUERY_STRATEGY_GUIDANCE}

AGENT RULES:
1. Search only for the specific unknown. Do not paste the whole user task as the query.
2. Identify language/framework/package/version from local files before searching when possible.
3. Let provider_policy=auto filter inactive providers. Do not name a provider unless operation=providers confirms it is active.
4. For exact errors, use strategy=error_debugging or strategy=exact and include package/framework/version.
5. For semantic providers, use strategy=semantic/hyde/step_back, not keyword soup.
6. For official docs, use strategy=official_docs and optionally restrict domains to one or two authoritative domains.
7. Do not hard-restrict to many domains. Multiple site: filters can destroy recall for keyword engines.
8. Use query_plan=multi when a task benefits from separate keyword, semantic, and synthesis queries.
9. Use operation=extract after discovery to read the most relevant official URL, changelog, issue, or article.
10. Cite URLs from returned results when relying on external facts in the final answer.
11. Check current_date. The tool response includes a top-level current_date field (YYYY-MM-DD format) and the tool block also carries it. Incorporate this date into your query to avoid targeting outdated years. Anti-pattern: "latest info on React 2024" when current_date is 2026 — this returns stale results. Correct pattern: use the current_date value to date-stamp queries.
`.trim()

let providerCache: { expiresAt: number; loadedAt: number; data: ProviderDiscovery } | undefined
let providerCachePromise: Promise<ProviderDiscovery> | undefined
const providerCooldowns = new Map<string, ProviderCooldown>()

function searchBinary() {
  const fallback = process.platform === "win32" ? "search.exe" : "search"
  return process.env.SEARCH_CLI_PATH?.trim() || fallback
}

function splitCsv(value: string | undefined): string[] {
  if (!value) return []
  return value
    .split(",")
    .map((v) => v.trim())
    .filter(Boolean)
}

function unique(values: string[]): string[] {
  return [...new Set(values)]
}

function clamp(n: number | undefined, fallback: number, min: number, max: number): number {
  if (!Number.isFinite(n)) return fallback
  return Math.max(min, Math.min(max, Math.trunc(n as number)))
}

function isUrl(value: string): boolean {
  try {
    const url = new URL(value)
    return url.protocol === "https:" || url.protocol === "http:"
  } catch {
    return false
  }
}

function looksLikeExactError(query: string): boolean {
  return /error|exception|panic|failed|traceback|stack trace|enoent|timeout|segfault|typeerror|referenceerror|unhandled|eaddrinuse|econnrefused|permission denied/i.test(query)
}

function providerList(providers: string | undefined): string[] {
  return splitCsv(providers).map((p) => p.toLowerCase())
}

function inferMode(operation: SearchOperation, requestedMode: SearchMode | undefined): SearchMode {
  if (operation === "extract") return "extract"
  if (operation === "scrape") return "scrape"
  if (operation === "similar") return "similar"
  return requestedMode || "auto"
}

function resolveFreshness(strategy: QueryStrategy, mode: SearchMode, requested: Freshness): Freshness {
  if (requested !== "auto") return requested
  if (strategy === "security") return "month"
  if (strategy === "release_notes" || strategy === "migration") return "year"
  if (mode === "news" || mode === "social") return "week"
  return "none"
}

function quoteForKeyword(query: string): string {
  if (query.includes('"')) return query
  if (looksLikeExactError(query) && query.length < 220) return `"${query}"`
  return query
}

function contextSuffix(taskContext?: string): string {
  const ctx = taskContext?.trim()
  return ctx ? ` Context: ${ctx}` : ""
}

function shapeQueryForCategory(rawQuery: string, strategy: QueryStrategy, category: ProviderCategory | undefined, taskContext?: string): string {
  const query = rawQuery.trim()
  const ctx = contextSuffix(taskContext)

  if (category === "extract" || category === "local_scrape") return query

  if (category === "keyword" || category === "vertical") {
    switch (strategy) {
      case "official_docs":
        return `${quoteForKeyword(query)} official documentation API reference guide${ctx}`
      case "release_notes":
        return `${quoteForKeyword(query)} release notes changelog breaking changes deprecation upgrade guide${ctx}`
      case "migration":
        return `${quoteForKeyword(query)} migration guide before after breaking changes compatibility examples${ctx}`
      case "error_debugging":
      case "exact":
        return `${quoteForKeyword(query)} fix workaround known issue${ctx}`
      case "security":
        return `${quoteForKeyword(query)} CVE advisory vulnerability mitigation patch release${ctx}`
      case "academic":
        return `${query} paper benchmark evaluation arxiv methodology${ctx}`
      default:
        return `${query}${ctx}`
    }
  }

  if (category === "semantic") {
    switch (strategy) {
      case "hyde":
        return `A technical document explaining ${query}, including correct APIs, version constraints, examples, common errors, migration notes, and edge cases.${ctx}`
      case "hype":
        return `Questions developers ask when trying to solve: ${query}. Include likely docs pages, examples, errors, pitfalls, and workarounds.${ctx}`
      case "step_back":
        return `Underlying concepts, official guidance, and design constraints needed to understand and solve: ${query}.${ctx}`
      case "official_docs":
        return `Official documentation and API reference explaining how to implement ${query}, with examples and constraints.${ctx}`
      case "migration":
        return `Migration documentation describing before and after behavior for ${query}, including compatibility risks and examples.${ctx}`
      case "error_debugging":
        return `A troubleshooting guide for ${query}, including root cause, known issues, edge cases, pitfalls, affected versions, and fixes.${ctx}`
      default:
        return `${query}${ctx}`
    }
  }

  if (category === "synthesis") {
    switch (strategy) {
      case "release_notes":
        return `What changed recently for ${query}? Focus on release notes, breaking changes, deprecations, and migration steps.${ctx}`
      case "security":
        return `Is there a current security advisory or CVE for ${query}? Include affected versions, mitigation, and patch releases.${ctx}`
      case "migration":
        return `What is the correct migration path for ${query}? Compare old and new APIs, risks, and examples.${ctx}`
      case "official_docs":
        return `What do the official docs say about ${query}? Include exact API names, configuration keys, and examples.${ctx}`
      case "error_debugging":
        return `How do developers fix ${query}? Include likely causes, official guidance, and known issue links.${ctx}`
      default:
        return `Find current, source-backed information needed to solve this coding task: ${query}.${ctx}`
    }
  }

  if (category === "social") {
    return `Search X/Twitter for recent developer reports, maintainer comments, outage chatter, or breaking-change discussion about: ${query}.${ctx}`
  }

  return `${query}${ctx}`
}

function parseJsonMaybe(text: string | undefined): any | undefined {
  const trimmed = text?.trim()
  if (!trimmed) return undefined
  try {
    return JSON.parse(trimmed)
  } catch {
    const first = trimmed.indexOf("{")
    const last = trimmed.lastIndexOf("}")
    if (first >= 0 && last > first) {
      try {
        return JSON.parse(trimmed.slice(first, last + 1))
      } catch {
        return undefined
      }
    }
    return undefined
  }
}

function truncateText(value: unknown, maxChars: number): unknown {
  if (typeof value !== "string") return value
  if (value.length <= maxChars) return value
  return `${value.slice(0, maxChars)}\n...[truncated ${value.length - maxChars} chars]`
}

function runSearchCli(binary: string, args: string[], timeoutMs: number, cwd: string, signal?: AbortSignal): Promise<{ stdout: string; stderr: string }> {
  return new Promise((resolve, reject) => {
    execFile(
      binary,
      args,
      {
        cwd,
        encoding: "utf8",
        timeout: timeoutMs,
        env: { ...process.env, PATH: process.env.PATH },
        maxBuffer: 12 * 1024 * 1024,
        windowsHide: true,
        shell: false,
        signal,
      },
      (err, stdout, stderr) => {
        if (err) {
          const e = err as ExecError
          e.stdout = stdout
          e.stderr = stderr
          reject(e)
          return
        }
        resolve({ stdout, stderr })
      },
    )
  })
}

function normalizeProviderName(value: string): string {
  return value.trim().toLowerCase()
}

function parseSimpleTomlKeys(content: string): Record<string, string> {
  const keys: Record<string, string> = {}
  let section = ""

  for (const rawLine of content.split(/\r?\n/)) {
    const trimmed = rawLine.trim()
    if (!trimmed || trimmed.startsWith("#")) continue

    const sectionMatch = trimmed.match(/^\[([^\]]+)]$/)
    if (sectionMatch) {
      section = sectionMatch[1].trim().toLowerCase()
      continue
    }

    if (section !== "keys") continue
    const eq = trimmed.indexOf("=")
    if (eq < 0) continue

    const key = trimmed.slice(0, eq).trim().toLowerCase()
    let value = trimmed.slice(eq + 1).trim()

    // Strip simple inline comments only when preceded by whitespace.
    value = value.replace(/\s+#.*$/, "").trim()
    if ((value.startsWith('"') && value.endsWith('"')) || (value.startsWith("'") && value.endsWith("'"))) {
      value = value.slice(1, -1)
    }
    if (key) keys[key] = value
  }

  return keys
}

function candidateConfigPaths(): string[] {
  const home = homedir()
  const paths = [
    process.env.SEARCH_CLI_CONFIG_PATH,
    process.env.SEARCH_CONFIG_PATH,
    process.env.XDG_CONFIG_HOME ? join(process.env.XDG_CONFIG_HOME, "search", "config.toml") : undefined,
    home ? join(home, ".config", "search", "config.toml") : undefined,
    home ? join(home, "Library", "Application Support", "search", "config.toml") : undefined,
    process.env.APPDATA ? join(process.env.APPDATA, "search", "config.toml") : undefined,
  ]
  return unique(paths.filter(Boolean) as string[])
}

function readConfigKeys(): { keys: Record<string, string>; path?: string; error?: string } {
  for (const candidate of candidateConfigPaths()) {
    try {
      if (!existsSync(candidate)) continue
      const content = readFileSync(candidate, "utf8")
      return { keys: parseSimpleTomlKeys(content), path: candidate }
    } catch (err: any) {
      return { keys: {}, path: candidate, error: String(err?.message || err) }
    }
  }
  return { keys: {} }
}

function configuredFromEnvOrConfig(provider: string, configKeys: Record<string, string>): boolean {
  if (provider === "stealth") return true
  if (configKeys[provider]?.trim()) return true
  return (PROVIDER_ENV_KEYS[provider] ?? []).some((key) => Boolean(process.env[key]?.trim()))
}

function getUserProviderOverride(): string[] | undefined {
  const raw = process.env.SEARCH_TOOL_ACTIVE_PROVIDERS || process.env.SEARCH_TOOL_AVAILABLE_PROVIDERS
  if (!raw?.trim()) return undefined
  return unique(splitCsv(raw).map(normalizeProviderName).filter((p) => (PROVIDERS as readonly string[]).includes(p)))
}

function getUserDisabledProviders(): Set<string> {
  return new Set(splitCsv(process.env.SEARCH_TOOL_DISABLED_PROVIDERS).map(normalizeProviderName))
}

function pruneExpiredCooldowns(now = Date.now()) {
  for (const [provider, cooldown] of providerCooldowns) {
    if (cooldown.expiresAt <= now) providerCooldowns.delete(provider)
  }
}

function cooldownFor(provider: string, now = Date.now()): ProviderCooldown | undefined {
  pruneExpiredCooldowns(now)
  const cooldown = providerCooldowns.get(provider)
  if (!cooldown || cooldown.expiresAt <= now) return undefined
  return cooldown
}

function buildLocalDiscovery(refresh = false): ProviderDiscovery {
  const loadedAt = Date.now()
  pruneExpiredCooldowns(loadedAt)

  const override = getUserProviderOverride()
  const disabled = getUserDisabledProviders()
  const config = readConfigKeys()
  const discoveryMethod: ProviderDiscovery["discovery_method"] = override ? "user_override" : "env_and_config_file"

  const configuredByKey = new Set<string>()
  for (const provider of PROVIDERS) {
    if (override) {
      if (override.includes(provider)) configuredByKey.add(provider)
    } else if (configuredFromEnvOrConfig(provider, config.keys)) {
      configuredByKey.add(provider)
    }
  }

  const activeProviders: ProviderStatus[] = []
  let hiddenCooldownCount = 0
  let hiddenUnconfiguredCount = 0

  for (const provider of PROVIDERS) {
    const isConfigured = configuredByKey.has(provider)
    const disabledByUser = disabled.has(provider)
    const cooldown = cooldownFor(provider, loadedAt)
    if (!isConfigured || disabledByUser || cooldown) {
      if (cooldown) hiddenCooldownCount += 1
      else hiddenUnconfiguredCount += 1
      continue
    }
    activeProviders.push({
      name: provider,
      configured: true,
      capabilities: PROVIDER_CAPABILITIES[provider] ?? [],
      categories: PROVIDER_CATEGORIES[provider] ?? [],
      env_keys: PROVIDER_ENV_KEYS[provider],
    })
  }

  const by_category = Object.fromEntries(CATEGORY_ORDER.map((c) => [c, []])) as Record<ProviderCategory, string[]>
  for (const provider of activeProviders) {
    for (const category of provider.categories) by_category[category].push(provider.name)
  }

  return {
    status: config.error ? "error" : "success",
    discovery_method: discoveryMethod,
    config_path: config.path,
    providers: activeProviders,
    configured: activeProviders.map((p) => p.name),
    by_category,
    cache_age_ms: refresh ? 0 : undefined,
    hidden_unconfigured_count: hiddenUnconfiguredCount,
    hidden_cooldown_count: hiddenCooldownCount,
    error: config.error,
  }
}

function discoverProviders(_binary: string, _cwd: string, _signal?: AbortSignal, refresh = false): Promise<ProviderDiscovery> {
  const now = Date.now()
  if (!refresh && providerCache && providerCache.expiresAt > now) {
    return Promise.resolve({ ...providerCache.data, cache_age_ms: now - providerCache.loadedAt })
  }
  if (!refresh && providerCachePromise) return providerCachePromise

  providerCachePromise = Promise.resolve().then(() => {
    const loadedAt = Date.now()
    const discovery = buildLocalDiscovery(refresh)
    providerCache = { expiresAt: Date.now() + PROVIDER_CACHE_TTL_MS, loadedAt, data: discovery }
    providerCachePromise = undefined
    return discovery
  })

  return providerCachePromise
}

function warmProviderCacheAtModuleLoad() {
  const loadedAt = Date.now()
  const discovery = buildLocalDiscovery(true)
  providerCache = { expiresAt: loadedAt + PROVIDER_CACHE_TTL_MS, loadedAt, data: discovery }
}

warmProviderCacheAtModuleLoad()

function compatibleProvidersForMode(mode: SearchMode, discovery: ProviderDiscovery): string[] {
  const active = new Set(discovery.configured)
  const supports = (provider: string, cap: string) => active.has(provider) && (PROVIDER_CAPABILITIES[provider] ?? []).includes(cap)

  if (mode === "extract" || mode === "scrape") {
    return PROVIDERS.filter((p) => active.has(p) && ((PROVIDER_CATEGORIES[p] ?? []).includes("extract") || (PROVIDER_CATEGORIES[p] ?? []).includes("local_scrape")))
  }
  if (mode === "similar") return PROVIDERS.filter((p) => supports(p, "similar"))
  if (mode === "social") return PROVIDERS.filter((p) => supports(p, "social"))
  if (mode === "scholar") return PROVIDERS.filter((p) => supports(p, "scholar"))
  if (mode === "patents") return PROVIDERS.filter((p) => supports(p, "patents"))
  if (mode === "images") return PROVIDERS.filter((p) => supports(p, "images"))
  if (mode === "places") return PROVIDERS.filter((p) => supports(p, "places"))
  if (mode === "people") return PROVIDERS.filter((p) => supports(p, "people"))
  if (mode === "academic") return PROVIDERS.filter((p) => supports(p, "academic"))
  if (mode === "news") return PROVIDERS.filter((p) => supports(p, "news"))
  if (mode === "deep") return PROVIDERS.filter((p) => supports(p, "deep") || supports(p, "general"))
  return PROVIDERS.filter((p) => supports(p, "general") || supports(p, "deep"))
}

function fallbackActiveProviders(category: ProviderCategory, mode: SearchMode, discovery: ProviderDiscovery): string[] {
  const primary = categoryProviders(category, discovery)
  if (primary.length > 0) return primary
  return compatibleProvidersForMode(mode, discovery)
}

function categoryProviders(category: ProviderCategory, discovery: ProviderDiscovery): string[] {
  return discovery.by_category[category] ?? []
}

function configuredSubset(candidates: string[], discovery: ProviderDiscovery): string[] {
  const configured = new Set(discovery.configured)
  return candidates.filter((p) => configured.has(p))
}

function resolveRequestedProviders(rawProviders: string | undefined, discovery: ProviderDiscovery, policy: ProviderPolicy): { providers: string[]; warnings: string[]; errors: string[] } {
  const requested = providerList(rawProviders)
  const warnings: string[] = []
  const errors: string[] = []

  const invalid = requested.filter((p) => !(PROVIDERS as readonly string[]).includes(p))
  if (invalid.length > 0) warnings.push(`unknown provider override(s): ${invalid.join(", ")}`)

  const knownRequested = unique(requested.filter((p) => (PROVIDERS as readonly string[]).includes(p)))
  if (policy === "raw") return { providers: knownRequested, warnings, errors }

  const configured = configuredSubset(knownRequested, discovery)
  const unavailable = knownRequested.filter((p) => !configured.includes(p))
  if (unavailable.length > 0) {
    const msg = `requested provider(s) unavailable or unconfigured: ${unavailable.join(", ")}`
    if (policy === "strict") errors.push(msg)
    else warnings.push(`${msg}; filtered out by provider_policy=auto`)
  }
  return { providers: configured, warnings, errors }
}

function choosePrimaryCategory(strategy: QueryStrategy, mode: SearchMode, query: string): ProviderCategory {
  if (["extract", "scrape", "similar"].includes(mode)) return "extract"
  if (mode === "social") return "social"
  if (["scholar", "patents", "images", "places", "people"].includes(mode)) return "vertical"
  if (strategy === "hyde" || strategy === "semantic" || strategy === "step_back") return "semantic"
  if (strategy === "hype") return "synthesis"
  if (strategy === "exact" || strategy === "error_debugging" || looksLikeExactError(query)) return "keyword"
  if (strategy === "security" || strategy === "release_notes") return "keyword"
  if (strategy === "migration" || strategy === "official_docs") return "keyword"
  return "keyword"
}

function shouldUseMultiPlan(args: SearchArgs, strategy: QueryStrategy, mode: SearchMode, query: string): boolean {
  const plan = args.query_plan || "auto"
  if (plan === "single") return false
  if (plan === "multi") return true
  if (["extract", "scrape", "similar", "images", "places", "social"].includes(mode)) return false
  if (args.providers) return false
  if (strategy === "exact" || looksLikeExactError(query)) return false
  return ["official_docs", "release_notes", "migration", "security", "semantic", "hyde", "hype", "step_back", "academic"].includes(strategy)
}

function resolveFreshnessForCall(strategy: QueryStrategy, mode: SearchMode, requested: Freshness): Freshness {
  return resolveFreshness(strategy, mode, requested)
}

/**
 * Extract file-path suffixes from inline `site:` operators and route them properly:
 * - `site:domain/path` → domain added to `-d` flag, path becomes a regular query term
 * - `-site:domain/path` → path stripped, `-site:domain` kept in query, path becomes a term
 *
 * This prevents Brave API 422 errors (which rejects `/` in `site:` values) and ensures
 * path segments are preserved as search terms rather than silently dropped.
 */
function extractSiteOperators(rawQuery: string, existingDomains: string[]): { cleanQuery: string; domains: string[] } {
  const re = /(?<!\S)(-?site:)([^\s/]+)\/([^\s]+)/gi
  const domains = [...existingDomains]
  const extraTerms: string[] = []

  const cleanQuery = rawQuery.replace(re, (_full, operator, domain, path) => {
    if (operator.startsWith("-")) {
      // Negative site: keep operator in query but strip path
      extraTerms.push(path)
      return `-site:${domain}`
    }
    // Positive site: extract domain to -d flag, remove operator from query
    domains.push(domain)
    extraTerms.push(path)
    return ""
  })

  if (extraTerms.length === 0) return { cleanQuery, domains }

  const final = `${extraTerms.join(" ")} ${cleanQuery}`.replace(/\s+/g, " ").trim()
  return { cleanQuery: final, domains: unique(domains) }
}

function buildCliSearchArgs(query: string, mode: SearchMode, count: number, freshness: Freshness, providers: string[], domains: string[], excludes: string[]): string[] {
  const args = ["search", "-q", query, "-m", mode, "-c", String(count), "--json"]
  if (freshness !== "none" && !["extract", "scrape", "similar", "images", "places"].includes(mode)) {
    args.push("-f", freshness)
  }
  if (providers.length > 0) args.push("-p", unique(providers).join(","))
  if (domains.length > 0 && !["extract", "scrape", "similar", "images", "places", "social"].includes(mode)) {
    args.push("-d", unique(domains).join(","))
  }
  if (excludes.length > 0 && !["extract", "scrape", "similar", "social"].includes(mode)) {
    args.push("--exclude-domain", unique(excludes).join(","))
  }
  return args
}

function buildInvocations(input: Required<Pick<SearchArgs, "operation" | "mode" | "count" | "freshness" | "strategy" | "provider_policy">> & SearchArgs, discovery: ProviderDiscovery): { invocations: Invocation[]; errors: string[]; warnings: string[] } {
  const operation = input.operation
  const warnings: string[] = []
  const errors: string[] = []

  if (operation === "providers") return { invocations: [], errors, warnings }
  if (operation === "agent_info") return { invocations: [{ label: "agent_info", mode: "command", binaryArgs: ["agent-info", "--json"], warnings }], errors, warnings }
  if (operation === "config_check") return { invocations: [{ label: "config_check", mode: "command", binaryArgs: ["config", "check", "--json"], warnings }], errors, warnings }

  let query = input.query?.trim()
  if (!query) return { invocations: [], errors: ["query is required for search, extract, scrape, and similar operations"], warnings }

  const mode = inferMode(operation, input.mode)
  const freshness = resolveFreshnessForCall(input.strategy, mode, input.freshness)
  let domains = splitCsv(input.domains)

  // Extract site:domain/path operators so path segments don't cause 422 errors on providers
  // that validate site: values as plain domains (Brave).
  ;({ cleanQuery: query, domains } = extractSiteOperators(query, domains))
  const excludes = unique([...LOW_SIGNAL_EXCLUDE_DOMAINS, ...splitCsv(input.exclude_domains)])
  const requested = resolveRequestedProviders(input.providers, discovery, input.provider_policy)
  warnings.push(...requested.warnings)
  errors.push(...requested.errors)

  if (["extract", "scrape", "similar"].includes(mode) && !isUrl(query)) {
    warnings.push(`mode=${mode} normally expects query to be a URL`)
  }
  if (domains.length > 5) {
    warnings.push("domains contains more than 5 entries; hard domain restriction can overfilter keyword engines")
  }

  if (errors.length > 0) return { invocations: [], errors, warnings }

  if (input.providers || !shouldUseMultiPlan(input, input.strategy, mode, query)) {
    const category = choosePrimaryCategory(input.strategy, mode, query)
    const providers = input.providers
      ? requested.providers
      : input.provider_policy === "raw"
        ? []
        : fallbackActiveProviders(category, mode, discovery)

    if (input.provider_policy !== "raw" && input.providers && requested.providers.length === 0) {
      return { invocations: [], errors: ["no requested providers are active"], warnings }
    }
    if (input.provider_policy !== "raw" && !input.providers && providers.length === 0) {
      return { invocations: [], errors: ["no active providers support this operation or mode"], warnings }
    }

    const shapedQuery = shapeQueryForCategory(query, input.strategy, category, input.task_context)
    const binaryArgs = buildCliSearchArgs(shapedQuery, mode, input.count, freshness, providers, domains, excludes)
    return {
      invocations: [{ label: category, provider_category: category, providers, mode, shaped_query: shapedQuery, binaryArgs, warnings: [...warnings] }],
      errors,
      warnings,
    }
  }

  const calls: Array<{ category: ProviderCategory; mode: SearchMode; strategy: QueryStrategy; providers: string[]; label: string }> = []

  if (input.strategy === "academic") {
    calls.push(
      { category: "semantic", mode: "academic", strategy: "semantic", providers: categoryProviders("semantic", discovery), label: "academic_semantic" },
      { category: "vertical", mode: "scholar", strategy: "academic", providers: configuredSubset(["serper", "serpapi"], discovery), label: "scholar_keyword" },
    )
  } else if (input.strategy === "security") {
    calls.push(
      { category: "keyword", mode: "news", strategy: "security", providers: categoryProviders("keyword", discovery), label: "security_keyword_news" },
      { category: "synthesis", mode: "general", strategy: "security", providers: categoryProviders("synthesis", discovery), label: "security_synthesis" },
    )
  } else {
    calls.push(
      { category: "keyword", mode, strategy: input.strategy, providers: categoryProviders("keyword", discovery), label: "keyword" },
      { category: "semantic", mode, strategy: ["semantic", "hyde", "step_back"].includes(input.strategy) ? input.strategy : "semantic", providers: categoryProviders("semantic", discovery), label: "semantic" },
      { category: "synthesis", mode, strategy: input.strategy, providers: categoryProviders("synthesis", discovery), label: "synthesis" },
    )
  }

  const invocations: Invocation[] = []
  for (const call of calls) {
    const providers = input.provider_policy === "raw" ? call.providers : configuredSubset(call.providers, discovery)
    if (providers.length === 0) continue
    const shapedQuery = shapeQueryForCategory(query, call.strategy, call.category, input.task_context)
    invocations.push({
      label: call.label,
      provider_category: call.category,
      providers,
      mode: call.mode,
      shaped_query: shapedQuery,
      binaryArgs: buildCliSearchArgs(shapedQuery, call.mode, input.count, resolveFreshnessForCall(call.strategy, call.mode, input.freshness), providers, domains, excludes),
      warnings: [...warnings],
    })
    if (invocations.length >= MAX_AUTO_PLAN_CALLS) break
  }

  if (invocations.length === 0) {
    errors.push("no active providers are available for the requested search plan; run operation=providers or operation=config_check")
  }

  return { invocations, errors, warnings }
}

function quotaText(value: any): string {
  if (!value) return ""
  if (typeof value === "string") return value.toLowerCase()
  try {
    return JSON.stringify(value).toLowerCase()
  } catch {
    return String(value).toLowerCase()
  }
}

function shouldCooldownFailure(detail: any): boolean {
  const text = quotaText(detail)
  if (!text) return false
  if (text.includes("num_results_exceeded")) return false
  return /rate[_ -]?limit|too_many_requests|\b429\b|quota|credit|billing|insufficient_quota|monthly/.test(text)
}

function canonicalFailureProvider(provider: string): string {
  const normalized = normalizeProviderName(provider)
  if ((PROVIDERS as readonly string[]).includes(normalized)) return normalized
  if (normalized.startsWith("brave_")) return "brave"
  if (normalized.startsWith("serper_")) return "serper"
  if (normalized.startsWith("serpapi_")) return "serpapi"
  if (normalized.startsWith("exa_")) return "exa"
  if (normalized.startsWith("jina_")) return "jina"
  if (normalized.startsWith("firecrawl_")) return "firecrawl"
  if (normalized.startsWith("perplexity_")) return "perplexity"
  if (normalized.startsWith("you_")) return "you"
  if (normalized.startsWith("xai_")) return "xai"
  return normalized
}

function markProviderCooldown(provider: string, reason: string) {
  const normalized = canonicalFailureProvider(provider)
  if (!(PROVIDERS as readonly string[]).includes(normalized)) return
  providerCooldowns.set(normalized, {
    provider: normalized,
    expiresAt: Date.now() + PROVIDER_COOLDOWN_MS,
    reason: reason.slice(0, 500),
  })
  providerCache = undefined
}

function markCooldownsFromPayload(payload: any): string[] {
  const cooled: string[] = []
  const details = [
    ...(Array.isArray(payload?.metadata?.providers_failed_detail) ? payload.metadata.providers_failed_detail : []),
    ...(Array.isArray(payload?.providers_failed_detail) ? payload.providers_failed_detail : []),
  ]
  for (const detail of details) {
    const provider = String(detail?.provider || "").toLowerCase()
    if (!provider || !shouldCooldownFailure(detail)) continue
    markProviderCooldown(provider, quotaText(detail))
    cooled.push(provider)
  }

  const err = payload?.error
  const provider = String(err?.provider || err?.source || "").toLowerCase()
  if (provider && shouldCooldownFailure(err)) {
    markProviderCooldown(provider, quotaText(err))
    cooled.push(provider)
  }

  return unique(cooled)
}

function normalizeSearchPayload(payload: any, invocation: Invocation, maxSnippetChars: number) {
  const results = Array.isArray(payload?.results)
    ? payload.results.map((r: any) => ({
        title: r?.title ?? "",
        url: r?.url ?? "",
        source: r?.source ?? "",
        published: r?.published ?? undefined,
        snippet: truncateText(r?.snippet ?? "", maxSnippetChars),
        image_url: r?.image_url ?? undefined,
        extra: r?.extra ?? undefined,
        _call: invocation.label,
        _provider_category: invocation.provider_category,
      }))
    : []

  return {
    call: invocation.label,
    mode: payload?.mode ?? invocation.mode,
    query: payload?.query ?? invocation.shaped_query,
    provider_category: invocation.provider_category,
    providers_requested: invocation.providers,
    status: payload?.status ?? "success",
    metadata: payload?.metadata,
    results,
  }
}

function dedupeResults(results: any[]): any[] {
  const seen = new Set<string>()
  const out: any[] = []
  for (const result of results) {
    const key = String(result.url || `${result.title}:${result.source}`)
      .trim()
      .toLowerCase()
      .replace(/^http:\/\//, "https://")
      .replace(/^https:\/\/www\./, "https://")
      .replace(/\/$/, "")
    if (!key || seen.has(key)) continue
    seen.add(key)
    out.push(result)
  }
  return out
}

function suggestNextActions(status: string, results: any[], discovery: ProviderDiscovery): string[] {
  const out: string[] = []
  if (status === "all_providers_failed" || status === "error") {
    out.push("Run operation=config_check, then retry with a configured provider or lower count/freshness constraints.")
  }
  if (discovery.status === "error") {
    out.push("Provider discovery failed. Verify search-cli is installed and run: search providers --json.")
  }
  if (results.length === 0) {
    out.push("Try query_plan=multi, strategy=semantic, remove domain/freshness restrictions, or run operation=providers to inspect availability.")
  }
  const firstUsefulUrl = results.find((r) => typeof r.url === "string" && /^https?:\/\//.test(r.url))?.url
  if (firstUsefulUrl) {
    out.push(`Use operation=extract query=${firstUsefulUrl} to read the most relevant source before coding.`)
  }
  return out
}

function semanticExitCodeLabel(exitCode: number | string): string {
  switch (exitCode) {
    case 1:
      return "runtime_error"
    case 2:
      return "config_or_auth_error"
    case 3:
      return "bad_input"
    case 4:
      return "rate_limited"
    default:
      return "runtime_error"
  }
}

function normalizeCommandOutput(payload: any, includeRaw: boolean, toolDebug: Record<string, unknown>, discovery: ProviderDiscovery) {
  if (includeRaw) return JSON.stringify({ tool: toolDebug, provider_discovery: discovery, raw: payload }, null, 2)
  if (Array.isArray(payload?.providers)) {
    return JSON.stringify({ ...payload, provider_discovery: discovery, tool: toolDebug }, null, 2)
  }
  return JSON.stringify({ ...(payload && typeof payload === "object" ? payload : { raw_text: String(payload ?? "") }), provider_discovery: discovery, tool: toolDebug }, null, 2)
}

export default tool({
  description: DESCRIPTION,
  args: {
    operation: tool.schema
      .enum(["search", "extract", "scrape", "similar", "providers", "agent_info", "config_check"])
      .default("search")
      .describe("Operation to run. Use providers/config_check for diagnostics. Use extract/scrape/similar for URL-based workflows."),

    query: tool.schema
      .string()
      .optional()
      .describe("Search query or URL. Required for search/extract/scrape/similar. Do not paste the whole task; search the specific unknown."),

    mode: tool.schema
      .enum(SEARCH_MODES as unknown as [SearchMode, ...SearchMode[]])
      .default("auto")
      .describe("search-cli mode. Use deep for hard research, extract for known URLs, news for releases/CVEs, academic/scholar for papers."),

    count: tool.schema
      .number()
      .int()
      .min(1)
      .max(50)
      .default(10)
      .describe("Requested result count. Use 5-10 for targeted lookups, 15-25 for broad research. High counts may trigger provider limits."),

    providers: tool.schema
      .string()
      .optional()
      .describe("Comma-separated provider override. With provider_policy=auto, unavailable providers are filtered out before invoking search-cli."),

    domains: tool.schema
      .string()
      .optional()
      .describe("Comma-separated hard domain restriction, e.g. docs.rs,doc.rust-lang.org. Use sparingly; too many domains can overfilter."),

    exclude_domains: tool.schema
      .string()
      .optional()
      .describe("Additional comma-separated domains to exclude. A short low-signal coding-site denylist is already applied."),

    freshness: tool.schema
      .enum(["auto", "none", "day", "week", "month", "year"])
      .default("auto")
      .describe("Recency filter. auto uses week for news/social, month for security, year for migrations/releases, none otherwise."),

    strategy: tool.schema
      .enum([
        "auto",
        "exact",
        "semantic",
        "hyde",
        "hype",
        "step_back",
        "official_docs",
        "release_notes",
        "migration",
        "error_debugging",
        "security",
        "community",
        "academic",
      ])
      .default("auto")
      .describe("Query-shaping strategy. Use exact for errors, hyde/semantic for Exa, official_docs for docs, migration for upgrades."),

    query_plan: tool.schema
      .enum(["auto", "single", "multi"])
      .default("auto")
      .describe("single runs one shaped query. multi fans out separate keyword/semantic/synthesis queries. auto uses multi for migrations, docs, security, and semantic research."),

    provider_policy: tool.schema
      .enum(["auto", "strict", "raw"])
      .default("auto")
      .describe("auto filters unavailable providers, strict fails if requested providers are unavailable, raw bypasses provider filtering."),

    refresh_providers: tool.schema
      .boolean()
      .default(false)
      .describe("Refresh provider discovery cache before this call. Use after setting API keys or editing search-cli config."),

    task_context: tool.schema
      .string()
      .optional()
      .describe("Brief local context to append to shaped queries: language, framework, package version, OS, runtime, or failing command."),

    max_snippet_chars: tool.schema
      .number()
      .int()
      .min(500)
      .max(20_000)
      .default(DEFAULT_MAX_SNIPPET_CHARS)
      .describe("Maximum snippet/content characters per result. Use higher values for extract/scrape when reading a known URL."),

    timeout_ms: tool.schema
      .number()
      .int()
      .min(5_000)
      .max(MAX_TIMEOUT_MS)
      .default(DEFAULT_TIMEOUT_MS)
      .describe("CLI timeout in milliseconds. Increase for deep/perplexity/browserless if needed."),

    include_raw: tool.schema
      .boolean()
      .default(false)
      .describe("Return raw search-cli JSON instead of normalized/truncated result JSON."),
  },

  async execute(rawArgs: SearchArgs, context: any) {
    const started = Date.now()
    const currentDate = new Date().toISOString().slice(0, 10)
    const operation = rawArgs.operation || "search"
    const mode = rawArgs.mode || "auto"
    const strategy = rawArgs.strategy || "auto"
    const freshness = rawArgs.freshness || "auto"
    const queryPlan = rawArgs.query_plan || "auto"
    const providerPolicy = rawArgs.provider_policy || "auto"
    const count = clamp(rawArgs.count, 10, 1, 50)
    const timeoutMs = clamp(rawArgs.timeout_ms, DEFAULT_TIMEOUT_MS, 5_000, MAX_TIMEOUT_MS)
    const effectiveMode = inferMode(operation, mode)
    const maxSnippetChars = clamp(
      rawArgs.max_snippet_chars,
      ["extract", "scrape"].includes(effectiveMode) ? EXTRACT_MAX_SNIPPET_CHARS : DEFAULT_MAX_SNIPPET_CHARS,
      500,
      20_000,
    )

    const binary = searchBinary()
    const cwd = context?.worktree || context?.directory || process.cwd()
    const signal = context?.abort instanceof AbortSignal ? context.abort : undefined
    const discovery = await discoverProviders(binary, cwd, signal, Boolean(rawArgs.refresh_providers))

    if (operation === "providers") {
      return JSON.stringify(
        {
          version: "1",
          current_date: currentDate,
          status: discovery.status,
          provider_discovery: discovery,
          guidance: {
            availability_rule: "api key present in env/config means active; no provider API probes are made",
            cooldown_rule: "providers that return quota/rate-limit failures are hidden for this OpenCode process for 24 hours",
            routing_rule: "query fanout is adaptive and uses only active providers",
            date_rule: "The response includes current_date (YYYY-MM-DD). Incorporate this date into your query to avoid targeting outdated information.",
          },
        },
        null,
        2,
      )
    }

    const plan = buildInvocations(
      {
        ...rawArgs,
        operation,
        mode,
        count,
        freshness,
        strategy,
        query_plan: queryPlan,
        provider_policy: providerPolicy,
      },
      discovery,
    )

    const toolDebug: Record<string, unknown> = {
      binary,
      cwd,
      operation,
      mode,
      strategy,
      query_plan: queryPlan,
      provider_policy: providerPolicy,
      provider_discovery_status: discovery.status,
      active_providers: discovery.configured,
      warnings: plan.warnings,
      elapsed_ms: 0,
      current_date: currentDate,
    }

    if (plan.errors.length > 0) {
      toolDebug.elapsed_ms = Date.now() - started
      return JSON.stringify(
        {
          version: "1",
          current_date: currentDate,
          status: "error",
          error: {
            code: "bad_input_or_unavailable_provider",
            message: plan.errors.join("; "),
          },
          provider_discovery: discovery,
          tool: toolDebug,
        },
        null,
        2,
      )
    }

    const commandOnly = ["agent_info", "config_check"].includes(operation)
    const calls: any[] = []
    const allResults: any[] = []
    let aggregateStatus = "success"

    try {
      // Run all CLI invocations in parallel for multi-plan calls
      const promises = plan.invocations.map((invocation) =>
        runSearchCli(binary, invocation.binaryArgs, timeoutMs, cwd, signal).then(({ stdout, stderr }) => {
          const payload = parseJsonMaybe(stdout) ?? parseJsonMaybe(stderr) ?? stdout.trim()
          return { invocation, payload }
        })
      )

      let settled = await Promise.allSettled(promises)
      for (const result of settled) {
        if (result.status === "rejected") {
          // Propagate the rejection to the catch block
          throw result.reason
        }
        const { invocation, payload } = result.value

        if (commandOnly) {
          toolDebug.elapsed_ms = Date.now() - started
          toolDebug.invocations = plan.invocations.map((i) => ({ label: i.label, args: i.binaryArgs }))
          return normalizeCommandOutput(payload, Boolean(rawArgs.include_raw), toolDebug, discovery)
        }

        const cooledProviders = markCooldownsFromPayload(payload)
        if (cooledProviders.length > 0) {
          invocation.warnings.push(`provider(s) placed into 24h cooldown after quota/rate-limit failure: ${cooledProviders.join(", ")}`)
        }

        const normalized = normalizeSearchPayload(payload, invocation, maxSnippetChars)
        calls.push(normalized)
        allResults.push(...normalized.results)
        if (["all_providers_failed", "error"].includes(normalized.status)) aggregateStatus = normalized.status
        else if (normalized.status === "partial_success" && aggregateStatus === "success") aggregateStatus = "partial_success"
      }

      const results = dedupeResults(allResults)
      toolDebug.elapsed_ms = Date.now() - started
      toolDebug.invocations = plan.invocations.map((i) => ({
        label: i.label,
        category: i.provider_category,
        providers: i.providers,
        mode: i.mode,
        shaped_query: i.shaped_query,
        args: i.binaryArgs,
        warnings: i.warnings,
      }))

      const finalDiscovery = providerCooldowns.size > 0 ? await discoverProviders(binary, cwd, signal, true) : discovery

      if (rawArgs.include_raw) {
        return JSON.stringify({ tool: toolDebug, provider_discovery: finalDiscovery, calls }, null, 2)
      }

      const estimatedProviderCalls = plan.invocations.reduce((sum, inv) => sum + inv.providers.length, 0)

      return JSON.stringify(
        {
          version: "1",
          current_date: currentDate,
          status: results.length === 0 && aggregateStatus === "success" ? "no_results" : aggregateStatus,
          estimated_provider_calls: estimatedProviderCalls,
          provider_discovery: finalDiscovery,
          calls,
          results,
          result_count: results.length,
          tool: toolDebug,
          next_actions: suggestNextActions(aggregateStatus, results, finalDiscovery),
        },
        null,
        2,
      )
    } catch (err: any) {
      const elapsed = Date.now() - started
      const execErr = err as ExecError
      const parsed = parseJsonMaybe(execErr.stdout) ?? parseJsonMaybe(execErr.stderr)
      const exitCode = execErr.status ?? execErr.code ?? 1
      const isTimeout =
        execErr.killed === true ||
        execErr.signal === "SIGTERM" ||
        String(execErr.code) === "ETIMEDOUT" ||
        elapsed >= timeoutMs - 250

      toolDebug.elapsed_ms = elapsed
      toolDebug.invocations = plan.invocations.map((i) => ({ label: i.label, args: i.binaryArgs, shaped_query: i.shaped_query, providers: i.providers }))

      if (parsed && typeof parsed === "object") {
        markCooldownsFromPayload(parsed)
        const finalDiscovery = providerCooldowns.size > 0 ? await discoverProviders(binary, cwd, signal, true) : discovery
        return JSON.stringify({ ...parsed, provider_discovery: finalDiscovery, tool: toolDebug }, null, 2)
      }

      const notFound = execErr.code === "ENOENT"
      return JSON.stringify(
        {
          version: "1",
          current_date: currentDate,
          status: "error",
          error: {
            code: notFound ? "binary_not_found" : isTimeout ? "timeout" : semanticExitCodeLabel(exitCode),
            message: notFound
              ? "search-cli binary was not found. Install agent-search or set SEARCH_CLI_PATH to the search binary."
              : String(execErr.stderr || execErr.message || "search-cli failed").slice(0, 4000),
            exit_code: exitCode,
            suggestion: notFound
              ? "Install with: cargo install agent-search. Then run: search agent-info. Or set SEARCH_CLI_PATH=/absolute/path/to/search."
              : isTimeout
                ? "Retry with fewer providers/results, a narrower mode, query_plan=single, or a larger timeout_ms."
                : "Run operation=config_check or operation=providers to inspect search-cli setup and provider availability.",
          },
          provider_discovery: discovery,
          tool: toolDebug,
        },
        null,
        2,
      )
    }
  },
})
