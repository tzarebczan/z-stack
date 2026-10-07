import { defineConfig } from "vitest/config";
export default defineConfig({
  test: { include: ["packages/base/test/**/*.test.ts"], maxWorkers: 2, minWorkers: 1 },
});
