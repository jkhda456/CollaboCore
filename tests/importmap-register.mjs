// node --import ./tests/importmap-register.mjs ...   (see importmap-hooks.mjs)
import { register } from "node:module";
register("./importmap-hooks.mjs", import.meta.url);
