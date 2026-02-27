#!/usr/bin/env -S deno run -A

import {
  argument,
  command,
  constant,
  object,
  optional,
  option,
} from "jsr:@optique/core@0.4.4/parser"
import { run } from "jsr:@optique/run@0.4.4"
import { choice, string } from "jsr:@optique/core@0.4.4/valueparser"
import { parse as parseToml } from "jsr:@std/toml@1"

interface AppConfig {
  default?: string
  author?: string
  year?: string
  token?: string
}

interface License {
  key: string
  body: string
}

interface LicenseListItem {
  key?: string
}

const APP_NAME = "license-add"
const HOME_DIR = Deno.env.get("HOME") ?? Deno.cwd()
const CONFIG_DIR = `${HOME_DIR}/.config/${APP_NAME}`
const CACHE_DIR = `${HOME_DIR}/.cache/${APP_NAME}`
const CONFIG_PATH = `${CONFIG_DIR}/config.toml`
const GITHUB_LICENSE_API = "https://api.github.com/licenses"
const CURRENT_YEAR = new Date().getFullYear().toString()

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

const fetchAllLicenses = async (token?: string): Promise<Map<string, License>> => {
  const response = await fetch(GITHUB_LICENSE_API, {
    headers: createHeaders(token),
  })

  if (!response.ok) {
    const body = await response.text()
    throw new Error(`Failed to fetch licenses: ${response.status} ${response.statusText}\n${body}`)
  }

  const items = (await response.json()) as ReadonlyArray<LicenseListItem>
  const keys = items
    .map((item) => item.key)
    .filter((key): key is string => typeof key === "string" && key.length > 0)

  const results = await Promise.allSettled(keys.map((key) => fetchLicense(key, token)))

  for (const result of results) {
    if (result.status === "fulfilled") {
      await writeCachedLicense(result.value)
    }
  }

  const licenses = new Map<string, License>()

  for (const result of results) {
    if (result.status === "fulfilled") {
      licenses.set(result.value.key, result.value)
    }
  }

  return licenses
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

const ensureCache = async (token?: string): Promise<void> => {
  try {
    await fetchAllLicenses(token)
  } catch (error) {
    const existingKeys = await getCachedLicenseKeys()
    if (existingKeys.length === 0) {
      throw error
    }
  }
}

const readConfig = async (): Promise<AppConfig> => {
  try {
    const text = await Deno.readTextFile(CONFIG_PATH)
    const parsed = parseToml(text) as Record<string, unknown>

    const namespace =
      typeof parsed.license === "object" &&
        parsed.license !== null &&
        !Array.isArray(parsed.license)
        ? (parsed.license as Record<string, unknown>)
        : {}

    const getString = (obj: Record<string, unknown>, key: string): string | undefined => {
      const value = obj[key]
      return typeof value === "string" ? value : undefined
    }

    return {
      default: getString(namespace, "default") ?? getString(parsed, "default"),
      author: getString(namespace, "author") ?? getString(parsed, "author"),
      year: getString(namespace, "year") ?? getString(parsed, "year") ?? CURRENT_YEAR,
      token: getString(namespace, "token") ?? getString(parsed, "token"),
    }
  } catch (error) {
    if (error instanceof Deno.errors.NotFound) {
      return { year: CURRENT_YEAR }
    }

    throw error
  }
}

const createKeyParser = (keys: readonly string[], strict: boolean) => {
  const normalized = [...new Set(keys)].sort()

  const description = normalized.length > 0
    ? [{
      type: "text" as const,
      text: `Allowed keys: ${normalized.join(", ")}`,
    }]
    : undefined

  if (normalized.length === 0 || !strict) {
    return optional(argument(string({ metavar: "KEY" }), description ? { description } : undefined))
  }

  return optional(
    argument(choice(normalized, { metavar: "KEY" }), {
      description,
    }),
  )
}

const createBootstrapParser = (keys: readonly string[]) =>
  command(
    "add",
    object({
      action: constant("add"),
      key: createKeyParser(keys, false),
      author: optional(option("-a", "--author", string({ metavar: "NAME" }))),
      year: optional(option("-y", "--year", string({ metavar: "YEAR" }))),
      token: optional(option("-t", "--token", string({ metavar: "TOKEN" }))),
    }),
  )

const createCliParser = (keys: readonly string[]) =>
  command(
    "add",
    object({
      action: constant("add"),
      key: createKeyParser(keys, true),
      author: optional(option("-a", "--author", string({ metavar: "NAME" }))),
      year: optional(option("-y", "--year", string({ metavar: "YEAR" }))),
      token: optional(option("-t", "--token", string({ metavar: "TOKEN" }))),
    }),
  )

const getLicenseByKey = async (key: string): Promise<License> => {
  const cached = await readCachedLicense(key)
  if (cached) {
    return cached
  }

  throw new Error(`License \"${key}\" not found in cache. Run without network restrictions first to refresh cache.`)
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
  Deno.chdir(CONFIG_DIR)

  const config = await readConfig()

  const cachedKeys = await getCachedLicenseKeys()

  const bootstrapArgs = run(createBootstrapParser(cachedKeys), {
    args: Deno.args,
    help: "both",
    programName: "license.ts",
  })

  const effectiveToken = bootstrapArgs.token ?? config.token

  await ensureCache(effectiveToken)
  const refreshedKeys = await getCachedLicenseKeys()
  const cliParser = createCliParser(refreshedKeys)

  const args = run(cliParser, {
    args: Deno.args,
    help: "both",
    programName: "license.ts",
  })

  const licenseKey = args.key ?? config.default
  if (!licenseKey) {
    console.error(
      "License key is required. Provide one as an argument or config default in ~/.config/license-add/config.toml",
    )
    Deno.exit(1)
  }

  const author = args.author ?? config.author
  const year = resolveYear(args.year ?? config.year)

  const license = await getLicenseByKey(licenseKey)
  let text = license.body

  if (year) {
    text = replaceYear(year, license.key, text)
  }

  if (author) {
    text = replaceAuthor(author, license.key, text)
  }

  console.log(text)
}

if (import.meta.main) {
  await main()
}
