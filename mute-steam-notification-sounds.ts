#!/usr/bin/env -S deno run --allow-env --allow-read --allow-write

const DEFAULT_SOUND_NAMES = [
  "friend_online.wav",
  "friend_join.wav",
  "message.wav",
] as const
const HOME_DIR = Deno.env.get("HOME") ?? Deno.cwd()
const BACKUP_DIR_NAME = ".backup-sounds"
const SAMPLE_RATE = 44_100
const CHANNELS = 2
const BYTES_PER_SAMPLE = 2
const SILENT_FRAMES = 4_410

interface Options {
  all: boolean
  dryRun: boolean
  restore: boolean
  steamDir?: string
  soundNames: string[]
}

const usage =
  `Usage: mute-steam-notification-sounds.ts [options] [sound.wav ...]

Replaces Steam friend notification sounds with silent WAV files.
Backups are kept in friends/${BACKUP_DIR_NAME}/ and reused for restore.

Options:
  --steam-dir <path>  Steam directory (default: auto-detect native Steam)
  --all               Mute or restore every friends/*.wav file
  --restore           Restore backed-up sound files
  --dry-run           Print actions without changing files
  -h, --help          Show this help
`

const fail = (message: string): never => {
  console.error(`error: ${message}`)
  Deno.exit(1)
}

const expandHome = (path: string): string => {
  if (path === "~") return HOME_DIR
  if (path.startsWith("~/")) return `${HOME_DIR}/${path.slice(2)}`
  return path
}

const exists = async (path: string): Promise<boolean> => {
  try {
    await Deno.stat(path)
    return true
  } catch (error) {
    if (error instanceof Deno.errors.NotFound) return false
    throw error
  }
}

const parseArgs = (args: string[]): Options => {
  const options: Options = {
    all: false,
    dryRun: false,
    restore: false,
    soundNames: [],
  }

  for (let index = 0; index < args.length; index += 1) {
    const arg = args[index]

    switch (arg) {
      case "--all":
        options.all = true
        break
      case "--dry-run":
        options.dryRun = true
        break
      case "--restore":
        options.restore = true
        break
      case "--steam-dir": {
        const value = args[index + 1]
        if (!value) fail("--steam-dir requires a path")
        options.steamDir = expandHome(value)
        index += 1
        break
      }
      case "-h":
      case "--help":
        console.log(usage)
        Deno.exit(0)
      default:
        if (arg.startsWith("--steam-dir=")) {
          options.steamDir = expandHome(arg.slice("--steam-dir=".length))
        } else if (arg.startsWith("-")) {
          fail(`unknown option: ${arg}`)
        } else {
          options.soundNames.push(arg)
        }
    }
  }

  return options
}

const discoverSteamDir = async (explicit?: string): Promise<string> => {
  if (explicit) return explicit

  const candidates = [
    Deno.env.get("STEAM_DIR"),
    `${HOME_DIR}/.local/share/Steam`,
    `${HOME_DIR}/.steam/steam`,
  ].filter((value): value is string => Boolean(value))

  for (const candidate of candidates.map(expandHome)) {
    if (await exists(`${candidate}/friends`)) return candidate
  }

  return `${HOME_DIR}/.local/share/Steam`
}

const readAllSoundNames = async (friendsDir: string): Promise<string[]> => {
  const names: string[] = []

  for await (const entry of Deno.readDir(friendsDir)) {
    if (entry.isFile && entry.name.toLowerCase().endsWith(".wav")) {
      names.push(entry.name)
    }
  }

  return names.sort()
}

const selectedSoundNames = async (
  friendsDir: string,
  options: Options,
): Promise<string[]> => {
  if (options.all) return readAllSoundNames(friendsDir)
  if (options.soundNames.length > 0) return [...new Set(options.soundNames)]
  return [...DEFAULT_SOUND_NAMES]
}

const writeUint16 = (view: DataView, offset: number, value: number) => {
  view.setUint16(offset, value, true)
}

const writeUint32 = (view: DataView, offset: number, value: number) => {
  view.setUint32(offset, value, true)
}

const ascii = (buffer: Uint8Array, offset: number, value: string) => {
  for (let index = 0; index < value.length; index += 1) {
    buffer[offset + index] = value.charCodeAt(index)
  }
}

const createSilentWav = (): Uint8Array => {
  const dataSize = SILENT_FRAMES * CHANNELS * BYTES_PER_SAMPLE
  const buffer = new Uint8Array(44 + dataSize)
  const view = new DataView(buffer.buffer)

  ascii(buffer, 0, "RIFF")
  writeUint32(view, 4, 36 + dataSize)
  ascii(buffer, 8, "WAVE")
  ascii(buffer, 12, "fmt ")
  writeUint32(view, 16, 16)
  writeUint16(view, 20, 1)
  writeUint16(view, 22, CHANNELS)
  writeUint32(view, 24, SAMPLE_RATE)
  writeUint32(view, 28, SAMPLE_RATE * CHANNELS * BYTES_PER_SAMPLE)
  writeUint16(view, 32, CHANNELS * BYTES_PER_SAMPLE)
  writeUint16(view, 34, BYTES_PER_SAMPLE * 8)
  ascii(buffer, 36, "data")
  writeUint32(view, 40, dataSize)

  return buffer
}

const copyIfMissing = async (
  source: string,
  destination: string,
  dryRun: boolean,
) => {
  if (await exists(destination)) return
  console.log(`backup ${source} -> ${destination}`)
  if (!dryRun) await Deno.copyFile(source, destination)
}

const muteSound = async (
  friendsDir: string,
  backupDir: string,
  soundName: string,
  dryRun: boolean,
) => {
  const source = `${friendsDir}/${soundName}`
  const backup = `${backupDir}/${soundName}`

  if (!(await exists(source))) {
    console.log(`skip missing ${source}`)
    return
  }

  if (!dryRun) await Deno.mkdir(backupDir, { recursive: true })
  await copyIfMissing(source, backup, dryRun)
  console.log(`mute ${source}`)

  if (!dryRun) {
    await Deno.writeFile(source, createSilentWav())
    await Deno.chmod(source, 0o644)
  }
}

const restoreSound = async (
  friendsDir: string,
  backupDir: string,
  soundName: string,
  dryRun: boolean,
) => {
  const target = `${friendsDir}/${soundName}`
  const backup = `${backupDir}/${soundName}`

  if (!(await exists(backup))) {
    console.log(`skip missing backup ${backup}`)
    return
  }

  console.log(`restore ${backup} -> ${target}`)
  if (!dryRun) await Deno.copyFile(backup, target)
}

const main = async () => {
  const options = parseArgs(Deno.args)
  const steamDir = await discoverSteamDir(options.steamDir)
  const friendsDir = `${steamDir}/friends`
  const backupDir = `${friendsDir}/${BACKUP_DIR_NAME}`

  if (!(await exists(friendsDir))) {
    fail(`Steam friends sound directory not found: ${friendsDir}`)
  }

  const names = await selectedSoundNames(friendsDir, options)
  if (names.length === 0) fail("no sound files selected")

  for (const name of names) {
    if (name.includes("/") || name.includes("\0")) {
      fail(`invalid sound file name: ${name}`)
    }

    if (options.restore) {
      await restoreSound(friendsDir, backupDir, name, options.dryRun)
    } else {
      await muteSound(friendsDir, backupDir, name, options.dryRun)
    }
  }

  if (!options.restore) {
    console.log("restart Steam if a notification sound was already cached")
  }
}

if (import.meta.main) await main()
