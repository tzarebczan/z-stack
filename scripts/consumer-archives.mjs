import { copyFileSync, mkdirSync, readFileSync, statSync } from "node:fs";
import { join, resolve } from "node:path";

/** Copy this run's prebuilt archives; without --archives, callers still pack locally. */
export function reuseConsumerArchives(root, destination, names, args = process.argv.slice(2)) {
  if (args.includes("--archives")) throw new Error("Use --archives=DIR");
  const options = args.filter(arg => arg.startsWith("--archives="));
  if (!options.length) return false;
  if (options.length !== 1 || !options[0].slice(11)) throw new Error("Use one --archives=DIR option");
  const source = resolve(options[0].slice(11));
  const version = JSON.parse(readFileSync(join(root, "package.json"))).version;
  const files = names.map(name => `z-stack-${name}-${version}.tgz`);
  // Check the full set before copying. An incomplete set must never silently repack.
  for (const file of files) {
    try {
      if (!statSync(join(source, file)).isFile()) throw new Error();
    } catch {
      throw new Error(`Missing prebuilt archive ${file} in ${source}`);
    }
  }
  mkdirSync(destination, { recursive: true });
  for (const file of files) copyFileSync(join(source, file), join(destination, file));
  console.log(`Reusing ${files.length} consumer archives from this build`);
  return true;
}
