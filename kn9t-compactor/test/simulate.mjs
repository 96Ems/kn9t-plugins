// Simulated-host harness: drives the built kn9t-compactor plugin over stdio
// and asserts the compactor_compact round trip (session_read → triage+summary
// in parallel → plan with verbatim kept tool result).
import { spawn } from "node:child_process";
import assert from "node:assert/strict";

const proc = spawn("node", ["dist/main.js"], { stdio: ["pipe", "pipe", "inherit"] });
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

async function waitFor(pred, what, timeoutMs = 15000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const idx = got.findIndex(pred);
    if (idx >= 0) return got.splice(idx, 1)[0];
    await sleep(10);
  }
  throw new Error(`timeout waiting for ${what}; got: ${JSON.stringify(got)}`);
}

const SPAN = [
  { seq: 1, role: "user", content: [{ type: "text", text: "fix the bug" }] },
  { seq: 2, role: "assistant", content: [{ type: "tool_call", id: "t1", name: "bash", args_json: "{\"cmd\":\"ls\"}" }] },
  { seq: 3, role: "assistant", content: [{ type: "tool_result", id: "t1", is_error: false, content: [{ type: "text", text: "file1 file2" }] }] },
  { seq: 4, role: "assistant", content: [{ type: "tool_call", id: "t2", name: "bash", args_json: "{\"cmd\":\"ls /tmp\"}" }] },
  { seq: 5, role: "assistant", content: [{ type: "tool_result", id: "t2", is_error: false, content: [{ type: "text", text: "scratch noise (bbbb\ncccc)" }] }] },
  // t3 is deliberately left out of the model's plan: the plugin must keep it,
  // not drop it (fail-safe against losing a tool result).
  { seq: 6, role: "assistant", content: [{ type: "tool_call", id: "t3", name: "read", args_json: "{\"path\":\"src/main.rs\"}" }] },
  { seq: 7, role: "assistant", content: [{ type: "tool_result", id: "t3", is_error: false, content: [{ type: "text", text: "fn main() {}" }] }] },
];

send({ t: "hello", proto: 1, kn9t: "0.1.0-test" });

const hello = await waitFor((m) => m.t === "hello" && m.name, "plugin hello");
assert.deepEqual(hello.capabilities.sort(), ["compactor", "host_api"].sort(), "capabilities");
console.log("✓ hello:", hello.name, hello.capabilities.join(","));

send({ t: "hook", id: 42, hook: "compactor_compact", payload: { session: "sess-x", model: { provider: "test", id: "m1" }, replaced: { start: 1, end: 3 } } });

// Drive requests until the final result lands.
// The compactor now sends triage and summary in PARALLEL, so we may see
// multiple provider_complete requests before responding to any.
let triageCalls = 0;
let summaryReqId = null;
const uiStates = [];
const deadline = Date.now() + 15000;

while (Date.now() < deadline) {
  await sleep(10);
  
  // Process all pending requests
  const req = got.find((m) => m.t === "request");
  if (req) {
    got.splice(got.indexOf(req), 1);
    
    if (req.op === "session_read") {
      assert.equal(req.payload.session, "sess-x");
      send({ t: "api_result", id: req.id, ok: true, result: { messages: SPAN } });
    } else if (req.op === "ui_register_lua" || req.op === "ui_set_state" || req.op === "ui_clear") {
      // UI updates are fire-and-forget, just ack them
      if (req.op === "ui_set_state") uiStates.push(req.payload.state);
      send({ t: "api_result", id: req.id, ok: true, result: null });
    } else if (req.op === "provider_complete") {
      assert.equal(req.payload.session, "sess-x", "provider_complete carries session");
      assert.ok(Array.isArray(req.payload.messages) && req.payload.messages.length === 2, "two messages");
      assert.ok(Array.isArray(req.payload.tools) && req.payload.tools.length === 1, "one tool declared");
      const toolName = req.payload.tools[0].name;
      const sys = req.payload.messages[0].content[0].text;
      const usr = req.payload.messages[1].content[0].text;
      
      let reply;
      if (toolName === "submit_triage") {
        triageCalls++;
        if (triageCalls === 1) {
          // Model ignores the tool and answers in prose — must NOT crash; the
          // plugin spends its correction shot instead (sync retry).
          reply = { content: [{ type: "text", text: "I think we should keep t1 and drop t2." }] };
        } else {
          // Retry carries the correction instruction, then a real tool call.
          assert.ok(usr.includes("call submit_triage exactly once"), "correction shot instructs tool use");
          reply = { content: [{ type: "tool_call", id: "c1", name: "submit_triage", args_json: JSON.stringify({ decisions: [{ id: "t1", action: "keep" }, { id: "t2", action: "drop" }], resume_actions: ["run the fix"] }) }] };
        }
        send({ t: "api_result", id: req.id, ok: true, result: reply });
      } else if (toolName === "submit_summary") {
        // Summary runs in parallel with triage - respond immediately
        reply = { content: [{ type: "tool_call", id: "c2", name: "submit_summary", args_json: JSON.stringify({ summary: "all done — keep bash output" }) }] };
        send({ t: "api_result", id: req.id, ok: true, result: reply });
      } else {
        send({ t: "api_result", id: req.id, ok: false, error: `unknown tool ${toolName}` });
      }
    } else {
      send({ t: "api_result", id: req.id, ok: false, error: `unhandled op ${req.op}` });
    }
    continue;
  }
  
  const done = got.find((m) => m.t === "result" && m.id === 42);
  if (done) {
    assert.ok(!done.error, `compactor error: ${done.error}`);
    assert.equal(done.summary.role, "assistant");
    const textBlock = done.summary.content.find((b) => b.type === "text");
    assert.ok(textBlock.text.includes("all done"), "summary text from the model");
    const kept = done.summary.content.find((b) => b.type === "tool_result" && b.id === "t1");
    assert.ok(kept, "kept tool result embedded verbatim");
    assert.equal(kept.content[0].text, "file1 file2", "byte-exact kept output");
    assert.deepEqual(done.handoff.keep, ["t1", "t3"], "undecided t3 must be kept, not lost");
    assert.deepEqual(done.handoff.drop, ["t2"]);
    assert.deepEqual(done.handoff.resume_actions, ["run the fix"]);
    // The viewer must be able to tell which call a decision is about.
    const t1 = uiStates.flatMap((s) => s.decisions || []).find((d) => d.id === "t1");
    assert.equal(t1 && t1.preview, "ls", "decision rows must carry the tool call's command");
    // With parallel execution, triage may retry once if first attempt fails
    assert.ok(triageCalls >= 1, "triage was called at least once");
    console.log("✓ compactor_compact round trip OK (parallel triage+summary)");
    console.log("  summary content blocks:", done.summary.content.map((b) => b.type).join(", "));
    console.log("  handoff:", JSON.stringify(done.handoff));
    console.log("  triage calls:", triageCalls);
    proc.kill();
    process.exit(0);
  }
}
console.error("✗ no result within deadline; got:", JSON.stringify(got));
proc.kill();
process.exit(1);
