// Simulated-host harness for kn9t-subagent.
//
// Spawns the built plugin and plays the host over stdio: hello → tool_call →
// session_fork → session_prompt → done. It covers the paths that actually broke
// in live use, not just the happy round trip:
//
//   1. round trip            — the child's final answer comes back as the `done`
//                              body of the parent's tool call
//   2. fork is bare          — copy_events:false, budget + parent session set
//   3. nested re-entrancy    — a hook arriving while the plugin waits for a
//                              session_prompt reply is serviced inline, and the
//                              outer call still resolves (the 96E-17 deadlock)
//   4. context:"parent"      — session_read digest is prepended to the task
//   5. depth cap             — a chain deeper than KN9T_SUBAGENT_MAX_DEPTH is
//                              refused instead of hanging forever
import { spawn } from "node:child_process";
import assert from "node:assert/strict";

const MAX_DEPTH = 2;

const proc = spawn("node", ["dist/main.js"], {
  stdio: ["pipe", "pipe", "inherit"],
  env: { ...process.env, KN9T_SUBAGENT_MAX_DEPTH: String(MAX_DEPTH) },
});

let buf = "";
const got = [];
proc.stdout.on("data", (d) => {
  buf += d.toString("utf8");
  let i;
  while ((i = buf.indexOf("\n")) >= 0) {
    got.push(JSON.parse(buf.slice(0, i)));
    buf = buf.slice(i + 1);
  }
});

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const send = (m) => proc.stdin.write(JSON.stringify(m) + "\n");

async function take(pred, what, timeoutMs = 8000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const idx = got.findIndex(pred);
    if (idx >= 0) return got.splice(idx, 1)[0];
    await sleep(5);
  }
  throw new Error(`timeout waiting for ${what}; got: ${JSON.stringify(got)}`);
}

const isRequest = (op) => (m) => m.t === "request" && m.op === op;
const isDone = (id) => (m) => m.t === "done" && m.id === id;

const count = (m) => (m.t === "request" ? m : null);
const requests = () => got.filter(count).map((m) => m.op);

function reply(req, result, ok = true, error = undefined) {
  send({ t: "api_result", id: req.id, ok, result, error });
}

function textOf(done) {
  return (done.content || []).map((c) => c.text).join("\n");
}

async function main() {
  // ── handshake ──────────────────────────────────────────────────────────────
  send({ t: "hello", proto: 1, kn9t: "0.1.0-test" });
  const hello = await take((m) => m.t === "hello" && m.name, "plugin hello");
  assert.deepEqual(hello.capabilities.sort(), ["host_api", "streaming"].sort());
  const tool = hello.tools.find((t) => t.name === "subagent");
  assert.ok(tool, "declares the subagent tool");
  assert.ok(tool.description.includes("NO parent context"), "description states the child is contextless");
  assert.deepEqual(tool.schema.properties.context.enum, ["isolated", "parent"]);
  console.log("✓ hello: subagent tool, context: isolated|parent");

  // ── 1+2. round trip on a bare fork ─────────────────────────────────────────
  send({
    t: "hook",
    id: 1,
    hook: "tool_call",
    payload: { tool: "subagent", session: "sess-parent", args: { task: "summarize the auth module" } },
  });

  const read0 = got.find(isRequest("session_read"));
  assert.ok(!read0, "isolated context must NOT read the parent transcript");

  const fork = await take(isRequest("session_fork"), "session_fork");
  assert.equal(fork.payload.session, "sess-parent");
  assert.equal(fork.payload.copy_events, false, "child must be a bare fork, never a transcript copy");
  assert.equal(fork.payload.budget_usd, 0.5);
  reply(fork, { session: "child-1" });

  const prompt = await take(isRequest("session_prompt"), "session_prompt");
  assert.equal(prompt.payload.session, "child-1");
  assert.equal(prompt.payload.text, "summarize the auth module");
  assert.equal(prompt.payload.timeout_s, 240, "child watchdog must fire before the host's 300s tool timeout");
  reply(prompt, { session: "child-1", result: "auth module: token in a header" });

  const done1 = await take(isDone(1), "done #1");
  assert.equal(done1.is_error, false);
  assert.ok(textOf(done1).includes("auth module: token in a header"), "final answer is the tool result");
  assert.ok(textOf(done1).includes("child-1"), "child session id is reported");
  console.log("✓ round trip: bare fork → prompt → final answer as the tool result");

  // ── 3. nested re-entrancy: a hook serviced while blocked on a reply ────────
  send({
    t: "hook",
    id: 2,
    hook: "tool_call",
    payload: { tool: "subagent", session: "sess-parent", args: { task: "outer task" } },
  });
  const forkOuter = await take(isRequest("session_fork"), "outer fork");
  reply(forkOuter, { session: "child-A" });
  const promptOuter = await take(isRequest("session_prompt"), "outer prompt");
  assert.ok(promptOuter.payload.text.includes("outer task"));

  // The child (child-A) now calls subagent itself: the host sends the nested
  // hook while the plugin is still waiting for promptOuter's api_result.
  send({
    t: "hook",
    id: 3,
    hook: "tool_call",
    payload: { tool: "subagent", session: "child-A", args: { task: "inner task" } },
  });
  const forkInner = await take(isRequest("session_fork"), "inner fork");
  assert.equal(forkInner.payload.session, "child-A", "nested fork uses the child session");
  reply(forkInner, { session: "child-B" });
  const promptInner = await take(isRequest("session_prompt"), "inner prompt");
  assert.ok(promptInner.payload.text.includes("inner task"));
  reply(promptInner, { session: "child-B", result: "inner answer" });

  const done3 = await take(isDone(3), "done #3");
  assert.equal(done3.is_error, false);
  assert.ok(textOf(done3).includes("inner answer"), "nested call resolves");
  console.log("✓ nested: a hook during a pending reply is served inline");

  reply(promptOuter, { session: "child-A", result: "outer answer" });
  const done2 = await take(isDone(2), "done #2");
  assert.equal(done2.is_error, false);
  assert.ok(textOf(done2).includes("outer answer"), "outer call still resolves after the nested one");
  console.log("✓ nested: the outer call resolves once its reply lands");

  // ── 4. depth cap ───────────────────────────────────────────────────────────
  send({
    t: "hook",
    id: 4,
    hook: "tool_call",
    payload: { tool: "subagent", session: "sess-parent", args: { task: "level one" } },
  });
  const fork1 = await take(isRequest("session_fork"), "depth fork 1");
  reply(fork1, { session: "child-L1" });
  const prompt1 = await take(isRequest("session_prompt"), "depth prompt 1");
  assert.ok(prompt1.payload.text.includes("level one"));

  send({
    t: "hook",
    id: 5,
    hook: "tool_call",
    payload: { tool: "subagent", session: "child-L1", args: { task: "level two" } },
  });
  const fork2 = await take(isRequest("session_fork"), "depth fork 2");
  reply(fork2, { session: "child-L2" });
  const prompt2 = await take(isRequest("session_prompt"), "depth prompt 2");
  assert.ok(prompt2.payload.text.includes("level two"));

  // Depth is now 2 == MAX_DEPTH: the next nesting must be refused, and must NOT
  // issue a session_fork (which is what used to hang the whole tree).
  send({
    t: "hook",
    id: 6,
    hook: "tool_call",
    payload: { tool: "subagent", session: "child-L2", args: { task: "level three" } },
  });
  const done6 = await take(isDone(6), "depth refusal");
  assert.equal(done6.is_error, true);
  assert.ok(textOf(done6).includes("nesting depth"), "refused with a clear reason");
  assert.ok(!got.some(isRequest("session_fork")), "no fork is issued for the refused level");
  console.log("✓ depth cap: deeper nesting is refused, not queued");

  reply(prompt2, { session: "child-L2", result: "level two answer" });
  const done5 = await take(isDone(5), "done #5");
  assert.equal(done5.is_error, false);
  reply(prompt1, { session: "child-L1", result: "level one answer" });
  const done4 = await take(isDone(4), "done #4");
  assert.equal(done4.is_error, false);
  console.log("✓ depth cap: the unrefused levels still unwind cleanly");

  // ── 5. context:"parent" digest ─────────────────────────────────────────────
  send({
    t: "hook",
    id: 7,
    hook: "tool_call",
    payload: {
      tool: "subagent",
      session: "sess-parent",
      args: { task: "continue the fix", context: "parent" },
    },
  });
  const read = await take(isRequest("session_read"), "session_read for parent context");
  assert.equal(read.payload.session, "sess-parent");
  reply(read, {
    messages: [
      { seq: 1, role: "user", content: [{ type: "text", text: "fix the parser bug" }] },
      { seq: 2, role: "assistant", content: [{ type: "tool_call", id: "c1", name: "read", args_json: "{\"path\":\"src/parser.rs\"}" }] },
      { seq: 3, role: "tool", content: [{ type: "tool_result", id: "c1", content: [{ type: "text", text: "fn parse() {}" }] }] },
    ],
  });
  const fork7 = await take(isRequest("session_fork"), "parent-context fork");
  assert.equal(fork7.payload.copy_events, false, "context:parent is still a bare fork");
  reply(fork7, { session: "child-C" });
  const prompt7 = await take(isRequest("session_prompt"), "parent-context prompt");
  assert.ok(prompt7.payload.text.includes("fix the parser bug"), "digest carries parent user text");
  assert.ok(prompt7.payload.text.includes("src/parser.rs"), "digest carries parent tool calls");
  assert.ok(prompt7.payload.text.includes("continue the fix"), "task is preserved");
  reply(prompt7, { session: "child-C", result: "done" });
  const done7 = await take(isDone(7), "done #7");
  assert.equal(done7.is_error, false);
  console.log("✓ context:parent → text digest prepended to a bare child's task");

  console.log("\nall kn9t-subagent scenarios passed");
  proc.kill();
  process.exit(0);
}

main().catch((e) => {
  console.error("✗ kn9t-subagent simulate failed:", e.message);
  proc.kill();
  process.exit(1);
});
