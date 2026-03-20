#!/usr/bin/env -S deno run -A

import {
  argument,
  command,
  object,
  optional,
  option,
} from "jsr:@optique/core@0.10.6/parser"
import { runWith, type SourceContext } from "jsr:@optique/core@0.10.6/facade"
import { string } from "jsr:@optique/core@0.10.6/valueparser"
import {
  bindConfig,
  clearActiveConfig,
  configKey,
  createConfigContext,
  setActiveConfig,
} from "jsr:@optique/config@0.10.6"
import { parse as parseToml } from "jsr:@std/toml@1"
import * as v from "jsr:@valibot/valibot"

interface AppConfig {
  default?: string
  author?: string
  year?: string
  token?: string
  license?: {
    default?: string
    author?: string
    year?: string
    token?: string
  }
}

interface License {
  key: string
  body: string
}

const APP_NAME = "license-add"
const HOME_DIR = Deno.env.get("HOME") ?? Deno.cwd()
const CONFIG_DIR = `${HOME_DIR}/.config/${APP_NAME}`
const CACHE_DIR = `${HOME_DIR}/.cache/${APP_NAME}`
const CONFIG_PATH = `${CONFIG_DIR}/config.toml`
const GITHUB_LICENSE_API = "https://api.github.com/licenses"
const CURRENT_YEAR = new Date().getFullYear().toString()

const ConfigSchema = v.looseObject({
  default: v.optional(v.string()),
  author: v.optional(v.string()),
  year: v.optional(v.string()),
  token: v.optional(v.string()),
  license: v.optional(v.looseObject({
    default: v.optional(v.string()),
    author: v.optional(v.string()),
    year: v.optional(v.string()),
    token: v.optional(v.string()),
  })),
})

const createHeaders = (token?: string): Headers => {
  const headers = new Headers({
    "User-Agent": "license-script",
    "Accept": "application/vnd.github.v3+json",
  })

  if (token) {
    headers.set("Authorization", `token ${token}`)
  }

  return headers
}

const normalizeConfigPath = (value?: string): string => {
  if (!value || value.trim().length === 0) {
    return CONFIG_PATH
  }

  const path = value.trim()

  if (path === "~") {
    return HOME_DIR
  }

  if (path.startsWith("~" + "/")) {
    return `${HOME_DIR}/${path.slice(2)}`
  }

  return path
}

const ensureDirs = async () => {
  await Deno.mkdir(CONFIG_DIR, { recursive: true })
  await Deno.mkdir(CACHE_DIR, { recursive: true })
}

const cachePathFor = (key: string): string => `${CACHE_DIR}/${encodeURIComponent(key)}.txt`

const readCachedLicense = async (key: string): Promise<License | undefined> => {
  try {
    const body = await Deno.readTextFile(cachePathFor(key))
    return { key, body }
  } catch (error) {
    if (error instanceof Deno.errors.NotFound) {
      return undefined
    }

    throw error
  }
}

const writeCachedLicense = async (license: License): Promise<void> => {
  await Deno.writeTextFile(cachePathFor(license.key), license.body)
}

const fetchLicense = async (key: string, token?: string): Promise<License> => {
  const response = await fetch(`${GITHUB_LICENSE_API}/${encodeURIComponent(key)}`, {
    headers: createHeaders(token),
  })

  if (!response.ok) {
    const body = await response.text()
    throw new Error(
      `Failed to fetch license \"${key}\": ${response.status} ${response.statusText}\n${body}`,
    )
  }

  return await response.json()
}

const getCachedLicenseKeys = async (): Promise<readonly string[]> => {
  try {
    const keys: string[] = []

    for await (const entry of Deno.readDir(CACHE_DIR)) {
      if (entry.isFile && entry.name.endsWith(".txt")) {
        const encodedKey = entry.name.slice(0, -4)
        try {
          keys.push(decodeURIComponent(encodedKey))
        } catch {
          keys.push(encodedKey)
        }
      }
    }

    return [...new Set(keys)].sort()
  } catch (error) {
    if (error instanceof Deno.errors.NotFound) {
      return []
    }

    throw error
  }
}

const buildConfig = (parsed: unknown): AppConfig => {
  if (typeof parsed !== "object" || parsed === null) {
    return {}
  }

  const config = parsed as Record<string, unknown>
  const nested =
    typeof config.license === "object" &&
      config.license !== null &&
      !Array.isArray(config.license)
      ? config.license as Record<string, unknown>
      : {}

  const getString = (obj: Record<string, unknown>, key: string): string | undefined => {
    const value = obj[key]
    return typeof value === "string" ? value : undefined
  }

  return {
    default: getString(nested, "default") ?? getString(config, "default"),
    author: getString(nested, "author") ?? getString(config, "author"),
    year: getString(nested, "year") ?? getString(config, "year"),
    token: getString(nested, "token") ?? getString(config, "token"),
  }
}

const configContext = createConfigContext<AppConfig>({ schema: ConfigSchema })
const configCache = new Map<string, AppConfig | null>()

const getConfigForPath = async (path: string): Promise<AppConfig | null> => {
  const normalized = normalizeConfigPath(path)

  if (configCache.has(normalized)) {
    return configCache.get(normalized) ?? null
  }

  const loaded = await loadConfigFile(normalized)
  const stored = loaded ?? null
  configCache.set(normalized, stored)

  return stored
}

const extractConfigPathFromArgs = (args: readonly string[]): string | undefined => {
  for (let i = 0; i < args.length; i++) {
    const arg = args[i]
    if (arg === "--config") {
      const next = args[i + 1]
      if (next && !next.startsWith("-")) {
        return next
      }
      continue
    }

    if (arg.startsWith("--config=")) {
      return arg.slice("--config=".length)
    }
  }

  return undefined
}

const loadConfigFile = async (path: string): Promise<AppConfig | undefined> => {
  let text: string
  try {
    text = await Deno.readTextFile(path)
  } catch (error) {
    if (error instanceof Deno.errors.NotFound) {
      return undefined
    }

    throw error
  }

  const parsed = parseToml(text) as unknown
  const result = v.safeParse(ConfigSchema, parsed)

  if (!result.success) {
    const firstIssue = result.issues[0]
    const message = firstIssue ? `${firstIssue.path?.join(".") ?? ""}: ${firstIssue.message}` :
      "Invalid configuration"
    throw new Error(`Invalid config file (${path}): ${message}`)
  }

  return buildConfig(result.output)
}

;(configContext as {
  getAnnotations: (parsed?: unknown) => Promise<Record<symbol, unknown>>
}).getAnnotations = async (parsed?: unknown) => {
  const parsedConfigPath =
    typeof parsed === "object" && parsed !== null && "config" in parsed
      ? (parsed as { config?: string }).config
      : undefined

  const fallbackConfigPath = extractConfigPathFromArgs(Deno.args)
  const configPath = normalizeConfigPath(
    parsedConfigPath ?? fallbackConfigPath ?? `${CONFIG_DIR}/config.toml`,
  )
  const loaded = await getConfigForPath(configPath)

  if (!loaded) {
    return {} as Record<symbol, AppConfig>
  }

  setActiveConfig(configContext.id, loaded)
  return { [configKey]: loaded } as Record<symbol, AppConfig>
}

const createKeyParser = (keys: readonly string[]) => {
  const normalized = [...new Set(keys)].sort()

  const description = normalized.length > 0
    ? [{
      type: "text" as const,
      text: `Allowed keys: ${normalized.join(", ")}`,
    }]
    : undefined

  return optional(
    argument(string({ metavar: "KEY" }), description ? { description } : undefined),
  )
}

const createParser = (keys: readonly string[]) =>
  command(
    "add",
    object({
      config: optional(option("--config", string({ metavar: "PATH" }))),
      key: bindConfig(createKeyParser(keys), {
        context: configContext as any,
        key: "default",
        default: "",
      }),
      author: bindConfig(optional(option("-a", "--author", string({ metavar: "NAME" }))), {
        context: configContext as any,
        key: "author",
        default: "",
      }),
      year: bindConfig(option("-y", "--year", string({ metavar: "YEAR" })), {
        context: configContext as any,
        key: "year",
        default: CURRENT_YEAR,
      }),
      token: bindConfig(optional(option("-t", "--token", string({ metavar: "TOKEN" }))), {
        context: configContext as any,
        key: "token",
        default: "",
      }),
    }),
  )

const getLicenseByKey = async (key: string, token?: string): Promise<License> => {
  const cached = await readCachedLicense(key)
  if (cached) {
    return cached
  }

  try {
    const fetched = await fetchLicense(key, token)
    await writeCachedLicense(fetched)
    return fetched
  } catch (error) {
    if (error instanceof Error) {
      throw new Error(
        `License \"${key}\" is not cached and could not be fetched. ${error.message}`,
      )
    }

    throw error
  }
}

const replaceAuthor = (author: string, key: string, text: string): string => {
  switch (key) {
    case "agpl-3.0":
    case "gpl-2.0":
    case "gpl-3.0":
    case "lgpl-2.1":
      return text.replace(/<name of author>/g, author)
    case "apache-2.0":
      return text.replace(/[name of copyright owner]/g, author)
    case "bsd-2-clause":
    case "bsd-3-clause":
    case "mit":
    case "bsd-4-clause":
    case "isc":
      return text.replace(/[fullname]/g, author)
    case "wtfpl":
      return text.replace(/Sam Hocevar <sam@hocevar.net>/g, author)
    case "bsl-1.0":
    case "cc0-1.0":
    case "epl-2.0":
    case "mpl-2.0":
    case "unlicense":
    case "cc-by-4.0":
    case "lgpl-3.0":
    default:
      return text
  }
}

const replaceYear = (year: string, key: string, text: string): string => {
  switch (key) {
    case "agpl-3.0":
    case "gpl-2.0":
    case "gpl-3.0":
    case "lgpl-2.1":
      return text.replace(/<year>/g, year)
    case "apache-2.0":
      return text.replace(/[yyyy]/g, year)
    case "bsd-2-clause":
    case "bsd-3-clause":
    case "mit":
    case "bsd-4-clause":
    case "isc":
      return text.replace(/[year]/g, year)
    case "wtfpl": {
      let seen = 0
      return text.replace(/2004/g, (match) => {
        seen += 1
        return seen === 2 ? year : match
      })
    }
    case "bsl-1.0":
    case "cc0-1.0":
    case "epl-2.0":
    case "mpl-2.0":
    case "unlicense":
    case "cc-by-4.0":
    case "lgpl-3.0":
    default:
      return text
  }
}

const resolveYear = (rawYear?: string): string | undefined => {
  if (!rawYear || rawYear.trim() === "") {
    return CURRENT_YEAR
  }

  if (rawYear === "auto" || rawYear === "current") {
    return CURRENT_YEAR
  }

  return rawYear
}

const main = async () => {
  await ensureDirs()

  const configPathArg = extractConfigPathFromArgs(Deno.args)
  await getConfigForPath(configPathArg ?? CONFIG_PATH)

  const cachedKeys = await getCachedLicenseKeys()
  const parser = createParser(cachedKeys)

  const args = await runWith(parser, "license.ts", [configContext as SourceContext], {
    args: Deno.args,
    help: {
      mode: "both",
      onShow: () => Deno.exit(0),
    },
  })

  const token = args.token || undefined

  const licenseKey = args.key ?? ""
  if (!licenseKey) {
    throw new Error(
      "License key is required. Provide one as an argument or config default in ~/.config/license-add/config.toml",
    )
  }

  const license = await getLicenseByKey(licenseKey, token)

  let text = license.body
  const year = resolveYear(args.year)
  const author = args.author || undefined

  if (year) {
    text = replaceYear(year, license.key, text)
  }

  if (author) {
    text = replaceAuthor(author, license.key, text)
  }

  console.log(text)
  clearActiveConfig(configContext.id)
}

if (import.meta.main) {
  try {
    await main()
  } catch (error) {
    clearActiveConfig(configContext.id)
    if (error instanceof Error) {
      console.error(error.message)
    } else {
      console.error(String(error))
    }
    Deno.exit(1)
  }
}
