import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import { Context } from '@deepseek-ai/cordis'
import Include, { applyEntryPatches, entryListSchema, type PatchOptions } from '@deepseek-ai/cordis-plugin-include'
import Loader from '@deepseek-ai/cordis-plugin-loader'
import * as yaml from 'js-yaml'
import { afterEach, describe, expect, it } from 'vitest'

const PLUGIN_IDS = ['twin-memory', 'twin-soul', 'twin-dream', 'twin-companion']
const PLUGIN_NAMES = PLUGIN_IDS.map(id => `twin-bundle/${id}`)
let context: Context | undefined
let root: string | undefined

afterEach(async () => {
  await context?.fiber.dispose()
  context = undefined
  if (root !== undefined) await rm(root, { recursive: true, force: true })
  root = undefined
})

async function loadRows(profileDisabledId?: string): Promise<Context> {
  root = await mkdtemp(join(tmpdir(), 'dsh-twin-bundle-'))
  const patchPath = resolve(import.meta.dirname, '../cordis.patch.yml')
  const patches = yaml.load(await readFile(patchPath, 'utf8'), {
    schema: entryListSchema,
  }) as PatchOptions[]
  let entries = applyEntryPatches([], patches, () => {})
  if (profileDisabledId !== undefined) {
    entries = applyEntryPatches(entries, [{ id: profileDisabledId, disabled: true }], () => {})
  }

  const configPath = join(root, 'cordis.yml')
  await writeFile(configPath, yaml.dump(entries))
  const loaded = context = new Context()
  loaded.baseUrl = pathToFileURL(root).href + '/'
  await loaded.plugin(Loader)
  loaded.loader.builtins.include = Include
  loaded.loader.internal = undefined
  await loaded.loader.create({ name: 'cordis:include', config: { path: pathToFileURL(configPath).href } })
  await loaded.loader.await()
  for (const entry of loaded.loader.entries()) await entry.fiber?.await()
  return loaded
}

function twinEntries(ctx: Context) {
  return [...ctx.loader.entries()].filter(entry => PLUGIN_IDS.includes(entry.options.id ?? ''))
}

describe('twin-bundle plugin rows', () => {
  it('loads four separately named modules through Cordis Loader', async () => {
    const loaded = await loadRows()
    const rows = twinEntries(loaded)

    expect(rows.map(row => row.options.id)).toEqual(PLUGIN_IDS)
    expect(rows.map(row => row.options.name)).toEqual(PLUGIN_NAMES)
    expect(rows.every(row => row.fiber !== undefined)).toBe(true)
  })

  it('disables one profile row while its three siblings remain mounted', async () => {
    const loaded = await loadRows('twin-dream')
    const rows = twinEntries(loaded)

    expect(rows.map(row => row.options.id)).toEqual(PLUGIN_IDS)
    expect(rows.filter(row => row.fiber !== undefined).map(row => row.options.id)).toEqual([
      'twin-memory', 'twin-soul', 'twin-companion',
    ])
    expect(rows.find(row => row.options.id === 'twin-dream')?.options.disabled).toBe(true)
  })
})
