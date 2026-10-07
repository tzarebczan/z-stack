// Apply the demo's no-telemetry policy to dev, build and production CLI runs.
import { spawn } from "node:child_process";
import { createRequire } from "node:module";
const cli = createRequire(import.meta.url).resolve("next/dist/bin/next");
const child = spawn(process.execPath, [cli, ...process.argv.slice(2)], {
  stdio: "inherit", env: { ...process.env, NEXT_TELEMETRY_DISABLED: "1" },
});
child.on("error", () => { console.error("Could not start Next.js."); process.exitCode = 1; });
child.on("exit", code => { process.exitCode = code ?? 1; });
for (const signal of ["SIGINT", "SIGTERM"]) process.on(signal, () => child.kill(signal));
