import { strict as assert } from "node:assert";
import { spawn } from "node:child_process";
import { test } from "node:test";

import { adapterArgs, describe, listAttachable, parseList, serverOptions } from "../orbit";

test("the attach list is read as orbit prints it", () => {
  const list = parseList(
    JSON.stringify([
      { pid: 41, program: "/w/main.orb", args: ["-v"], listening: true, version: "0.0.0" },
      { pid: 42, program: "/w/other.lua", listening: false },
      { pid: 43, program: null, listening: false },
      { program: "no pid" },
    ]),
  );
  assert.deepEqual(list, [
    { pid: 41, program: "/w/main.orb", args: ["-v"], listening: true },
    { pid: 42, program: "/w/other.lua", args: [], listening: false },
    { pid: 43, program: null, args: [], listening: false },
  ]);
  assert.equal(describe(list[0]), "/w/main.orb -v");
  assert.equal(describe(list[2]), "(no script)");
  assert.throws(() => parseList("{}"));
});

test("the adapter runs orbit --debug-server or --debug-attach", () => {
  assert.deepEqual(adapterArgs("launch"), ["--debug-server"]);
  assert.deepEqual(adapterArgs("attach", 1234), ["--debug-attach", "1234"]);
  assert.deepEqual(adapterArgs("attach", "1234"), ["--debug-attach", "1234"]);
  assert.throws(() => adapterArgs("attach"), /pid/);
  assert.throws(() => adapterArgs("attach", "${command:pickProcess}"), /pid/);
});

test("blank search path entries are dropped", () => {
  assert.deepEqual(serverOptions(["lib/?.orb", " ", ""]), { searchPath: ["lib/?.orb"] });
});

// With ORBIT set to a built `orbit` (Linux), a program started with
// --debug-listen shows up in the list the picker offers.
test("a listening orbit program can be picked", { skip: !process.env.ORBIT }, async () => {
  const orbit = process.env.ORBIT as string;
  const child = spawn(orbit, ["--debug-listen", "-e", "while true do end"], { stdio: ["ignore", "ignore", "pipe"] });
  try {
    await new Promise<void>((resolve, reject) => {
      child.stderr.on("data", (d: Buffer) => {
        if (d.toString().includes("debugger listening")) resolve();
      });
      child.on("exit", () => reject(new Error("orbit exited")));
    });
    const found = await listAttachable(orbit);
    const me = found.find((a) => a.pid === child.pid);
    assert.ok(me, JSON.stringify(found));
    assert.equal(me.listening, true);
  } finally {
    child.kill();
  }
});
