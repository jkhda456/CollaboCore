// The *Node* runtime's command line, including Windows-style paths.
//   node --test tests/cli-args.test.mjs
//
// The shipped runtime is the native engine now (its own command line is covered by
// `engine/build.sh --tests`), and runtime/src is kept only as the reference implementation, so
// this skips itself unless that code can be loaded (it needs @lowland/guest on the path).
import assert from "node:assert/strict";
import { test } from "node:test";

const parseArgs = await import("../runtime/src/main.mjs").then((m) => m.parseArgs, () => undefined);
if (parseArgs === undefined) {
  test.skip("the Node runtime is not built here", () => {});
}

test("mounts: unix, Windows drive letters, read-only, repeated", { skip: parseArgs === undefined }, () => {
  const { config } = parseArgs([
    "--mount", "/home/me/proj:/work",
    "--mount", "C:\\Users\\me\\docs:/docs:ro",
    "--mount", "D:\\data:/data",
    "--mount", "./rel:/rel:ro",
  ]);
  assert.deepEqual(config.mounts, [
    { hostPath: "/home/me/proj", guestPath: "/work", readOnly: false },
    { hostPath: "C:\\Users\\me\\docs", guestPath: "/docs", readOnly: true },
    { hostPath: "D:\\data", guestPath: "/data", readOnly: false },
    { hostPath: "./rel", guestPath: "/rel", readOnly: true },
  ]);
});

test("modes and the rest of the options", { skip: parseArgs === undefined }, () => {
  assert.equal(parseArgs([]).mode, "shell");
  assert.equal(parseArgs(["--stdio"]).mode, "stdio");
  const exec = parseArgs(["exec", "--cpus", "3", "--quiet", "--no-python", "--no-network", "--host-exec", "allow", "--", "python3", "-c", "print(1)"]);
  assert.equal(exec.mode, "exec");
  assert.deepEqual(exec.command, ["python3", "-c", "print(1)"]);
  assert.deepEqual(exec.config, { cpus: 3, quiet: true, python: false, network: false, hostExec: "allow" });
  const net = parseArgs(["--allow", "a.example", "--allow", "*.b.example", "--deny", "c.example"]);
  assert.deepEqual(net.config.network, { allow: ["a.example", "*.b.example"], deny: ["c.example"] });
});

test("bad input is rejected with a message, not silently", { skip: parseArgs === undefined }, () => {
  assert.throws(() => parseArgs(["--mount", "no-guest-path"]), /--mount expects HOST:GUEST/);
  assert.throws(() => parseArgs(["--mount"]), /needs a value/);
  assert.throws(() => parseArgs(["--nope"]), /unknown argument/);
});
