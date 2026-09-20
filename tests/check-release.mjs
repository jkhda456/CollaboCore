// Static check of release/: every module the page loads must resolve (through its import
// map) to a file that exists, and nothing reachable may import a Node builtin — a browser
// cannot load those. Catches broken bundles before a browser does.
//   node tests/check-release.mjs
import { existsSync, readFileSync } from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const release = resolve(dirname(fileURLToPath(import.meta.url)), "../dist/web");
const html = readFileSync(join(release, "index.html"), "utf8");
const imports = JSON.parse(html.match(/<script type="importmap">([\s\S]*?)<\/script>/)[1]).imports;
const inline = html.match(/<script type="module">([\s\S]*?)<\/script>/)[1];

const problems = [];
const seen = new Set();
// Comments (including JSDoc examples that show `import ... from "node:net"`) are not imports.
const strip_comments = (src) => src.replace(/\/\*[\s\S]*?\*\//g, "").replace(/^\s*\/\/.*$/gm, "");
const specifiers = (src) =>
  [...strip_comments(src).matchAll(/(?:^|[\s;}])(?:import|export)\s*(?:[^"';]*?\sfrom\s*)?["']([^"']+)["']/g)].map(
    (m) => m[1],
  );

function resolveSpec(spec, fromDir) {
  if (spec.startsWith("node:")) return { node: spec };
  if (spec in imports) return { file: join(release, imports[spec]) };
  if (spec.startsWith(".")) return { file: resolve(fromDir, spec) };
  return { missing: spec };
}

function visit(file, why) {
  if (seen.has(file)) return;
  seen.add(file);
  if (!existsSync(file)) return problems.push(`missing file ${relative(release, file)} (${why})`);
  if (!file.endsWith(".js")) return;
  for (const spec of specifiers(readFileSync(file, "utf8"))) {
    const r = resolveSpec(spec, dirname(file));
    if (r.node) problems.push(`${relative(release, file)} imports ${r.node}`);
    else if (r.missing) problems.push(`${relative(release, file)} imports unmapped bare specifier "${r.missing}"`);
    else visit(r.file, `imported by ${relative(release, file)}`);
  }
}

for (const spec of specifiers(inline)) {
  const r = resolveSpec(spec, release);
  if (r.file) visit(r.file, "imported by index.html");
  else problems.push(`index.html imports unresolved "${spec}"`);
}
for (const [spec, target] of Object.entries(imports)) visit(join(release, target), `import map ${spec}`);

console.log(`${seen.size} modules reachable from index.html`);
if (problems.length) {
  console.error(problems.map((p) => "PROBLEM: " + p).join("\n"));
  process.exit(1);
}
console.log("release import graph ok");
