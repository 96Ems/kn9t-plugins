/**
 * kn9t-subagent — spawn sub-agent sessions using kn9t primitives.
 *
 * Provides the `subagent` tool. A sub-agent is nothing more than a session
 * running a turn (R-PLUG-110): this plugin
 *
 *   1. forks a **bare** child session (`session_fork` with `copy_events: false`)
 *   2. runs the task as one synchronous turn (`session_prompt`)
 *   3. returns the child's final answer as the tool result of the parent's call
 *
 * Why the child is always bare
 * ----------------------------
 * The parent's assistant message carrying the very tool call being executed is
 * already in the transcript, and its `tool_result` does not exist yet. Forking
 * with `copy_events: true` hands the child a transcript that ends in an
 * unanswered `tool_call`, which real providers reject (OpenAI and Anthropic both
 * require a `tool_result` immediately after `tool_use`). The child also inherits
 * the parent's whole context, which makes it re-mimic the parent's delegation
 * instead of doing the task.
 *
 * So the child starts with no context. Everything it needs must be in `task` —
 * or requested explicitly with `context: "parent"`, which prepends a bounded
 * *text* digest of the parent conversation (never the raw transcript, so no
 * dangling tool call can reach the provider).
 *
 * NO custom agent loop — uses kn9t's native ReAct via `session_prompt`.
 */

import * as fs from "node:fs";

// ── Wire Protocol ────────────────────────────────────────────────────────────

class LineReader {
  private buf = Buffer.alloc(0);

  readLine(): string | null {
    while (true) {
      const nl = this.buf.indexOf(0x0a);
      if (nl >= 0) {
        const line = this.buf.subarray(0, nl).toString("utf8");
        this.buf = this.buf.subarray(nl + 1);
        return line;
      }
      const chunk = Buffer.alloc(65536);
      const n = fs.readSync(0, chunk, 0, chunk.length, null);
      if (n <= 0) return null;
      this.buf = Buffer.concat([this.buf, chunk.subarray(0, n)]);
    }
  }
}

function writeMsg(msg: unknown): void {
  fs.writeSync(1, JSON.stringify(msg) + "\n");
}

interface ApiResult {
  t: "api_result";
  id: number;
  ok: boolean;
  result?: Record<string, unknown>;
  error?: string;
}

interface HookMsg {
  t: "hook";
  id: number;
  hook: string;
  payload: Record<string, unknown>;
}

const reader = new LineReader();
const replies = new Map<number, ApiResult>();
let requestId = 1000;

// ── Host API Client ──────────────────────────────────────────────────────────

/**
 * Event pump: read lines until reply for `awaitId` arrives.
 * Handles incoming hooks inline (supports recursive subagents).
 *
 * The pump is the whole reason this plugin can nest: while it waits for one
 * `api_result` it keeps servicing `hook` messages, so a child's `subagent` call
 * is answered while the parent is still blocked. A blocking reader that
 * discarded the hooks it was not waiting for deadlocked the pair — do not
 * "simplify" this back into a plain `readLine` loop.
 */
function pumpUntil(awaitId: number): ApiResult {
  for (;;) {
    const hit = replies.get(awaitId);
    if (hit !== undefined) {
      replies.delete(awaitId);
      return hit;
    }
    const line = reader.readLine();
    if (line === null) throw new Error("host closed stdin");
    const msg = JSON.parse(line) as { t?: string; id?: number } & Record<string, unknown>;

    if (msg.t === "api_result" && typeof msg.id === "number") {
      replies.set(msg.id, msg as unknown as ApiResult);
      continue;
    }
    if (msg.t === "hook" && typeof msg.id === "number") {
      handleHook(msg.id, msg as unknown as HookMsg);
      continue;
    }
    if (msg.t === "shutdown") throw new Error("host shutdown");
    // Ignore other messages (events, etc.)
  }
}

/** Call a host API operation and wait for result. */
function hostCall(op: string, payload: Record<string, unknown>): ApiResult {
  const id = requestId++;
  writeMsg({ t: "request", id, op, payload });
  return pumpUntil(id);
}

// ── Progress ─────────────────────────────────────────────────────────────────

/**
 * Tool progress. Body fields are **flattened** at the top level (protocol
 * §5.12): `{"t":"chunk","id":N,"text":"..."}`. Wrapping it in a `body` object
 * made the host read `chunk["text"]` as absent and silently drop the progress.
 */
function sendProgress(hookId: number, text: string): void {
  writeMsg({ t: "chunk", id: hookId, text });
}

// ── Recursion ────────────────────────────────────────────────────────────────

/**
 * Maximum `subagent` nesting on one plugin process. A child inherits the tool
 * registry, `subagent` included, so without a cap a model that delegates
 * mimetically builds an unbounded tree: every level blocks on the one below and
 * the parent's tool call never returns. Recursion stays *possible* (it is the
 * plugin's choice, not a host hard-block), just bounded.
 *
 * Override with `KN9T_SUBAGENT_MAX_DEPTH`.
 */
const MAX_DEPTH = (() => {
  const raw = process.env.KN9T_SUBAGENT_MAX_DEPTH;
  const n = raw ? Number.parseInt(raw, 10) : Number.NaN;
  return Number.isFinite(n) && n > 0 ? n : 4;
})();

let depth = 0;

// ── Subagent Execution ───────────────────────────────────────────────────────

type SubagentContext = "isolated" | "parent";

interface SubagentArgs {
  task: string;
  model?: string;
  budget_usd?: number;
  tools?: string[];
  context?: SubagentContext;
  timeout_s?: number;
}

/**
 * The host gives a `tool_call` 300 s before it gives up (kn9t-plugin
 * `remote_tool.rs`). The child's watchdog must fire strictly *before* that, or
 * the host times out while the child is still running: the parent sees a
 * failure and the child keeps billing. 240 s leaves a margin for the fork,
 * the final reply and a slow host.
 */
const DEFAULT_TIMEOUT_S = 240;
const DEFAULT_BUDGET_USD = 0.5;

/** Character budget for the `context: "parent"` digest. */
const PARENT_DIGEST_CHARS = 12_000;

interface MessageWire {
  seq: number;
  role: string;
  content: Array<Record<string, unknown>>;
}

function textOf(content: Array<Record<string, unknown>>): string {
  return content
    .filter((b) => b["type"] === "text" && typeof b["text"] === "string")
    .map((b) => String(b["text"]))
    .join("\n");
}

function clip(s: string, max: number): string {
  return s.length <= max ? s : s.slice(0, max) + "…";
}

/**
 * A bounded, human-readable digest of the parent conversation. This is text,
 * not provider messages: it carries context to the child without ever handing
 * the provider the parent's in-flight, unanswered tool call.
 *
 * The tail is kept — recency is what a worker needs ("so far"), and the
 * beginning of a long transcript is the cheapest thing to drop.
 */
function parentDigest(messages: MessageWire[], budget = PARENT_DIGEST_CHARS): string {
  const lines: string[] = [];
  for (const m of messages) {
    const role = m.role.toUpperCase();
    for (const block of m.content) {
      const t = block["type"];
      if (t === "text") {
        const text = String(block["text"] ?? "");
        if (text.trim() !== "") lines.push(`[${role}] ${clip(text, 1500)}`);
      } else if (t === "tool_call") {
        lines.push(
          `[${role}] called ${String(block["name"] ?? "?")}(${clip(String(block["args_json"] ?? ""), 400)})`,
        );
      } else if (t === "tool_result") {
        const content = Array.isArray(block["content"])
          ? (block["content"] as Array<Record<string, unknown>>)
          : [];
        lines.push(`[tool ${String(block["id"] ?? "?")}] ${clip(textOf(content), 800)}`);
      }
      // thinking / image blocks are not useful context for a worker.
    }
  }
  const joined = lines.join("\n");
  if (joined.length <= budget) return joined;
  return "… (earlier conversation elided)\n" + joined.slice(joined.length - budget);
}

function executeSubagent(
  args: SubagentArgs,
  session: string | undefined,
  hookId: number,
): { content: Array<{ type: string; text: string }>; is_error: boolean } {
  const {
    task,
    model,
    budget_usd = DEFAULT_BUDGET_USD,
    tools,
    context = "isolated",
    timeout_s = DEFAULT_TIMEOUT_S,
  } = args;

  if (!task || task.trim() === "") {
    return {
      content: [{ type: "text", text: 'subagent requires non-empty "task"' }],
      is_error: true,
    };
  }
  if (!session) {
    return {
      content: [
        {
          type: "text",
          text: "subagent: the host did not send the calling session in the tool_call payload",
        },
      ],
      is_error: true,
    };
  }
  if (context !== "isolated" && context !== "parent") {
    return {
      content: [{ type: "text", text: `subagent: unknown context ${JSON.stringify(context)}` }],
      is_error: true,
    };
  }
  if (depth >= MAX_DEPTH) {
    return {
      content: [
        {
          type: "text",
          text:
            `subagent: nesting depth ${MAX_DEPTH} reached — do this task yourself instead of ` +
            `delegating again (raise KN9T_SUBAGENT_MAX_DEPTH to allow deeper trees).`,
        },
      ],
      is_error: true,
    };
  }

  depth += 1;
  try {
    sendProgress(hookId, `subagent: ${clip(task, 60)}`);

    // 1. Build the task text. `context: "parent"` prepends a text digest; the
    //    child session itself stays bare either way.
    let text = task;
    if (context === "parent") {
      const read = hostCall("session_read", { session });
      if (read.ok) {
        const messages = ((read.result as { messages?: MessageWire[] })?.messages ?? []) as MessageWire[];
        const digest = parentDigest(messages);
        if (digest.trim() !== "") {
          text =
            `Context — the conversation that led to this task (a digest; the raw ` +
            `transcript is not yours to see):\n\n${digest}\n\n---\n\nYour task:\n${task}`;
        }
      } else {
        sendProgress(hookId, `subagent: no parent context (${read.error ?? "session_read failed"})`);
      }
    }

    // 2. Fork a bare child: no transcript, but the parent's cwd and the budget
    //    snapshot (R-PLUG-130), and fork_reason=subagent so the child is
    //    auditable on its own.
    const forkPayload: Record<string, unknown> = {
      session,
      copy_events: false,
      budget_usd,
    };
    if (model) forkPayload.model = model;

    const fork = hostCall("session_fork", forkPayload);
    if (!fork.ok) {
      return {
        content: [{ type: "text", text: `subagent: session_fork failed: ${fork.error}` }],
        is_error: true,
      };
    }
    const childSession = fork.result?.session as string | undefined;
    if (!childSession) {
      return {
        content: [{ type: "text", text: "subagent: session_fork returned no session id" }],
        is_error: true,
      };
    }
    const shortId = childSession.substring(0, 8);
    sendProgress(hookId, `subagent session ${shortId}`);

    // 3. Run the task as one synchronous turn on the child (kn9t's own ReAct).
    const promptPayload: Record<string, unknown> = {
      session: childSession,
      text,
      timeout_s,
    };
    if (tools && tools.length > 0) promptPayload.tools = tools;

    const prompt = hostCall("session_prompt", promptPayload);
    if (!prompt.ok) {
      return {
        content: [
          {
            type: "text",
            text: `subagent ${shortId} failed: ${prompt.error}\n(session: ${childSession})`,
          },
        ],
        is_error: true,
      };
    }

    const answer = (prompt.result?.result as string) || "(sub-agent produced no text)";
    sendProgress(hookId, "subagent complete");

    return {
      content: [
        { type: "text", text: answer },
        {
          type: "text",
          text: `\n\n───────────────────────────────────────\n📎 Sub-agent session: ${childSession}`,
        },
      ],
      is_error: false,
    };
  } finally {
    depth -= 1;
  }
}

// ── Hook Handler ─────────────────────────────────────────────────────────────

function handleHook(id: number, msg: HookMsg): void {
  if (msg.hook !== "tool_call") {
    writeMsg({ t: "done", id, content: [], is_error: true });
    return;
  }

  const payload = msg.payload;
  const tool = payload.tool as string;
  const args = (payload.args as Record<string, unknown>) || {};
  const session = payload.session as string | undefined;

  if (tool !== "subagent") {
    writeMsg({
      t: "done",
      id,
      content: [{ type: "text", text: `unknown tool: ${tool}` }],
      is_error: true,
    });
    return;
  }

  const result = executeSubagent(args as unknown as SubagentArgs, session, id);
  writeMsg({ t: "done", id, ...result });
}

// ── Main ─────────────────────────────────────────────────────────────────────

const TOOL_DESCRIPTION =
  "Run a task in a separate sub-agent session and return its final answer. " +
  "The sub-agent starts with NO parent context: it sees only `task` and the " +
  "tools you grant, so write `task` as a self-contained brief (goal, exact " +
  "inputs, constraints, expected output, relevant paths) — it cannot see this " +
  "conversation. Pass context:'parent' only when the child genuinely needs the " +
  "conversation so far; it then receives a bounded text digest, never the raw " +
  "transcript. Use `tools` to give the child exactly the tools its job needs, " +
  "and `model` to pick a cheaper or stronger model for that job. Prefer " +
  "delegating independent or parallel work; nesting is capped.";

function main(): void {
  // Handshake
  const helloLine = reader.readLine();
  if (!helloLine) process.exit(1);

  const hello = JSON.parse(helloLine) as { t?: string; kn9t?: string };
  if (hello.t !== "hello") {
    console.error("kn9t-subagent: expected host hello");
    process.exit(1);
  }

  console.error(`kn9t-subagent: connected to kn9t ${hello.kn9t ?? "?"}`);

  writeMsg({
    t: "hello",
    name: "kn9t-subagent",
    capabilities: ["host_api", "streaming"],
    tools: [
      {
        name: "subagent",
        description: TOOL_DESCRIPTION,
        schema: {
          type: "object",
          additionalProperties: false,
          properties: {
            task: {
              type: "string",
              description:
                "Self-contained brief for the sub-agent: goal, inputs, constraints, expected output. The child has no other context.",
            },
            context: {
              type: "string",
              enum: ["isolated", "parent"],
              description:
                "'isolated' (default): the child starts with no parent context. " +
                "'parent': prepend a bounded digest of this conversation to the task.",
            },
            tools: {
              type: "array",
              items: { type: "string" },
              description:
                "Tool names the child may use (a filter over the live registry, not an addition). " +
                "Omit to inherit every visible tool.",
            },
            model: {
              type: "string",
              description: "Optional model id for the child (default: the parent's model).",
            },
            budget_usd: {
              type: "number",
              description: `Optional spend cap captured in the child's fork (default ${DEFAULT_BUDGET_USD} USD).`,
            },
            timeout_s: {
              type: "integer",
              description: `Optional child turn timeout in seconds (default ${DEFAULT_TIMEOUT_S}).`,
            },
          },
          required: ["task"],
        },
        parallel_safe: false,
      },
    ],
    hooks: [],
    events: [],
  });

  // Main loop
  while (true) {
    const line = reader.readLine();
    if (line === null) break;

    const msg = JSON.parse(line) as { t?: string; id?: number } & Record<string, unknown>;

    if (msg.t === "shutdown") break;

    if (msg.t === "hook" && typeof msg.id === "number") {
      handleHook(msg.id, msg as unknown as HookMsg);
    }
    // api_result without a waiter: ignore (stale)
  }
}

try {
  main();
} catch (e) {
  console.error("kn9t-subagent: fatal:", e);
  process.exit(1);
}
