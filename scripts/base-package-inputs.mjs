import { createHash } from 'node:crypto';
import { readdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';

/** Fingerprint the Base package sources/configuration, never environment or wallet data. */
export function basePackageInputs(directory) {
  const files = ['package.json', 'tsconfig.json'];
  function walk(relative) {
    for (const entry of readdirSync(join(directory, relative), { withFileTypes: true })) {
      if (entry.isDirectory()) walk(`${relative}/${entry.name}`);
      else if (entry.isFile() && entry.name.endsWith('.ts')) files.push(`${relative}/${entry.name}`);
    }
  }
  walk('src');
  const hash = createHash('sha256');
  for (const path of files.sort()) hash.update(path + '\0').update(readFileSync(join(directory, path))).update('\0');
  return hash.digest('hex');
}
