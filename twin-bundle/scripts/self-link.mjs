import { symlinkSync } from 'node:fs'

try {
  symlinkSync('..', 'node_modules/twin-bundle', 'dir')
} catch {
  // already linked
}
