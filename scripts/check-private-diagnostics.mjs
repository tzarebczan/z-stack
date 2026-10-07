import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import ts from "typescript";
import { fileURLToPath } from "node:url";

/** Source gate, not a proof about browser, dependency or infrastructure logs. */
export function privateDiagnosticViolations(roots, excluded = new Set()) {
  const violations = [];
  function walk(dir) {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const file = join(dir, entry.name);
      if (entry.isDirectory()) {
        if (!entry.name.startsWith("generated")) walk(file);
        continue;
      }
      if (!/\.tsx?$/.test(file) || /\.test\.|\.d\.ts$/.test(file) || excluded.has(entry.name)) continue;
      const source = ts.createSourceFile(file, readFileSync(file, "utf8"), ts.ScriptTarget.Latest, true);
      function visit(node) {
        if (ts.isPropertyAccessExpression(node) && ts.isIdentifier(node.expression) && node.expression.text === "console") {
          const call = node.parent;
          // Forbid payloads and passing a console method as a callback (e.g. catch(console.error)).
          if (!ts.isCallExpression(call) || call.expression !== node || call.arguments.length !== 1 || !ts.isStringLiteral(call.arguments[0])) {
            const { line } = source.getLineAndCharacterOfPosition(node.getStart(source));
            violations.push(`${file}:${line + 1}: console diagnostics must contain one fixed stage label`);
          }
        }
        ts.forEachChild(node, visit);
      }
      visit(source);
    }
  }
  roots.forEach((root) => walk(typeof root === "string" ? root : fileURLToPath(root)));
  return violations;
}

/** Conservative gate for the engine's ordinary tracing macros, including fields. */
export function nativeDiagnosticViolations(root) {
  const violations = [];
  function walk(dir) {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const file = join(dir, entry.name);
      if (entry.isDirectory()) { walk(file); continue; }
      if (!file.endsWith(".rs") || entry.name.includes("bench")) continue;
      const source = readFileSync(file, "utf8");
      const calls = /\b(?:tracing::)?(?:info|warn|debug|trace|error)!\s*\(/g;
      for (const match of source.matchAll(calls)) {
        // Reject everything except one literal. Braces would interpolate data.
        const rest = source.slice(match.index + match[0].length);
        if (!/^\s*"(?:[^"\\{}]|\\["\\nrt])*"\s*,?\s*\)/.test(rest)) {
          const line = source.slice(0, match.index).split("\n").length;
          violations.push(`${file}:${line}: native diagnostics must contain one fixed stage label`);
        }
      }
    }
  }
  walk(typeof root === "string" ? root : fileURLToPath(root));
  return violations;
}
