// Node module-resolution hook: resolves bare specifiers with the import map in
// release/index.html, exactly as the browser does. Tests then import the shipped files
// (host/ or web/*.js, release/static/**) without copying them or faking node_modules.
import { readFileSync } from "node:fs";
import { dirname, join, resolve as resolvePath } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const release = resolvePath(dirname(fileURLToPath(import.meta.url)), "../dist/web");
const html = readFileSync(join(release, "index.html"), "utf8");
const imports = JSON.parse(html.match(/<script type="importmap">([\s\S]*?)<\/script>/)[1]).imports;

export async function resolve(specifier, context, nextResolve) {
  if (Object.hasOwn(imports, specifier)) {
    return nextResolve(pathToFileURL(join(release, imports[specifier])).href, context);
  }
  return nextResolve(specifier, context);
}
