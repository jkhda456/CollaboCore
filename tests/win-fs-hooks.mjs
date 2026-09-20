// Test-only module hook: modules under dist/engine/guest see a `node:fs` whose `constants` look like
// Windows' (Node on Windows has no O_NOFOLLOW, O_NOCTTY, O_NONBLOCK, O_DIRECTORY, O_SYNC, O_DSYNC,
// O_DIRECT, O_NOATIME). Everything else is the real node:fs. Lets Linux CI exercise NodeFS's
// Windows code paths for open flags.
export async function resolve(specifier, context, next) {
  if (specifier === "node:fs" && context.parentURL?.includes("/dist/engine/guest/")) {
    return { url: "collabo-test:win-fs", shortCircuit: true };
  }
  return next(specifier, context);
}
export async function load(url, context, next) {
  if (url !== "collabo-test:win-fs") return next(url, context);
  return {
    format: "module",
    shortCircuit: true,
    source: `
      import * as fs from "node:fs";
      const absent = ["O_NOFOLLOW","O_NOCTTY","O_NONBLOCK","O_DIRECTORY","O_SYNC","O_DSYNC","O_DIRECT","O_NOATIME"];
      export const constants = Object.fromEntries(Object.entries(fs.constants).filter(([k]) => !absent.includes(k)));
      export const { promises, existsSync, readFileSync, statSync, lstatSync, readdirSync, realpathSync } = fs;
      export default { ...fs, constants };
    `,
  };
}
