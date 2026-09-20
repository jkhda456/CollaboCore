import { register } from "node:module";
register("./importmap-hooks.mjs", import.meta.url);
register("./win-fs-hooks.mjs", import.meta.url);
