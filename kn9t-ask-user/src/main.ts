/**
 * kn9t-ask-user — advanced question tool for kn9t.
 *
 * Supports multiple question types:
 * - text: free-form text input
 * - choice: single selection from options
 * - multi: multiple selection from options
 * - confirm: yes/no confirmation
 * - sequence: multiple questions in order
 *
 * Uses the generic `host_api` op `interaction_request` which emits
 * `LiveEvent::InteractionRequest` to the TUI. The TUI renders based on
 * the payload structure and POSTs the response to `/ui-respond`.
 */
import * as fs from "node:fs";

// ── Wire protocol ────────────────────────────────────────────────────────────

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
  t: string;
  id: number;
  ok: boolean;
  result?: Record<string, unknown>;
  error?: string;
}

let requestId = 1000;
const reader = new LineReader();
const replies = new Map<number, ApiResult>();

function pumpUntil(awaitId: number): ApiResult {
  for (;;) {
    const hit = replies.get(awaitId);
    if (hit !== undefined) {
      replies.delete(awaitId);
      return hit;
    }
    const line = reader.readLine();
    if (line === null) throw new Error("host closed");
    const msg = JSON.parse(line) as { t?: string; id?: number } & Record<string, unknown>;
    if (msg.t === "api_result" && typeof msg.id === "number") {
      replies.set(msg.id, msg as unknown as ApiResult);
      continue;
    }
    if (msg.t === "hook" && typeof msg.id === "number") {
      handleHook(msg.id, (msg.payload as Record<string, unknown>) ?? {});
      continue;
    }
    if (msg.t === "shutdown") throw new Error("shutdown");
  }
}

function hostRequest(op: string, payload: unknown): ApiResult {
  const id = requestId++;
  writeMsg({ t: "request", id, op, payload });
  return pumpUntil(id);
}

// ── Question types ───────────────────────────────────────────────────────────

interface QuestionOption {
  label: string;
  value?: string;
  description?: string;
}

interface BaseQuestion {
  header?: string;
  required?: boolean;
}

interface TextQuestion extends BaseQuestion {
  type: "text";
  question: string;
  placeholder?: string;
  default?: string;
}

interface ChoiceQuestion extends BaseQuestion {
  type: "choice";
  question: string;
  options: QuestionOption[];
  allow_custom?: boolean;
}

interface MultiQuestion extends BaseQuestion {
  type: "multi";
  question: string;
  options: QuestionOption[];
  min?: number;
  max?: number;
}

interface ConfirmQuestion extends BaseQuestion {
  type: "confirm";
  question: string;
  default?: boolean;
}

interface SequenceQuestion extends BaseQuestion {
  type: "sequence";
  questions: QuestionSpec[];
}

type QuestionSpec = TextQuestion | ChoiceQuestion | MultiQuestion | ConfirmQuestion | SequenceQuestion;

// ── Tool result ──────────────────────────────────────────────────────────────

interface ToolResult {
  content: Array<{ type: string; text: string }>;
  is_error: boolean;
}

function ok(text: string): ToolResult {
  return { content: [{ type: "text", text }], is_error: false };
}

function err(text: string): ToolResult {
  return { content: [{ type: "text", text }], is_error: true };
}

// ── Question execution ───────────────────────────────────────────────────────

// ── TUI display (plugin-supplied Lua) ────────────────────────────────────────
//
// The Lua *is* the question UI: it renders and drives the selection, answers
// through `kn9t.respond`, and is cleared when the question resolves.
// `placement = "bottom"` reserves rows between transcript and prompt, so
// answering never covers the model's message.

const UI_LUA = `
-- Local view state; reset whenever the question changes.
local S = {}
local seen = nil
local cursor = 1
local toggles = {}
local text = ""
local yes = true

local function is_text() return (S.kind or "text") == "text" end

local function option_at(i)
    local o = (S.options or {})[i]
    if type(o) == "table" then return o end
    if o == nil then return nil end
    return { label = o, value = o }
end

local function option_value(i)
    local o = option_at(i)
    return o and (o.value or o.label) or nil
end

local function option_label(i)
    local o = option_at(i)
    return o and (o.label or o.value) or ""
end

local function submit(payload)
    kn9t.respond(payload)
    return true
end

local function submit_current()
    local kind = S.kind or "text"
    if kind == "choice" then
        local v = option_value(cursor)
        if v == nil then return false end
        return submit({ value = v })
    elseif kind == "multi" then
        local values = {}
        for i = 1, #(S.options or {}) do
            if toggles[i] then table.insert(values, option_value(i)) end
        end
        return submit({ value = values })
    elseif kind == "confirm" then
        return submit({ value = yes })
    end
    return submit({ value = text })
end

local function move(delta)
    if (S.kind or "") == "confirm" then
        -- Yes/No is a vertical list: Up selects Yes, Down selects No.
        yes = delta < 0
        return
    end
    local n = #(S.options or {})
    if n == 0 then return end
    cursor = ((cursor - 1 + delta) % n) + 1
end

-- Exact keys; a handler returns false to fall through to on_text or the host.
kn9t.on_key("Up", function() if is_text() then return false end move(-1) return true end)
kn9t.on_key("k", function() if is_text() then return false end move(-1) return true end)
kn9t.on_key("Down", function() if is_text() then return false end move(1) return true end)
kn9t.on_key("j", function() if is_text() then return false end move(1) return true end)
kn9t.on_key("Tab", function() if is_text() then return false end move(1) return true end)

kn9t.on_key("Space", function()
    if is_text() then text = text .. " " return true end
    if (S.kind or "") == "multi" then toggles[cursor] = not toggles[cursor] end
    return true
end)

kn9t.on_key("Backspace", function()
    if not is_text() then return false end
    text = string.sub(text, 1, -2)
    return true
end)

kn9t.on_key("Left", function()
    if (S.kind or "") ~= "confirm" then return false end
    yes = true
    return true
end)

kn9t.on_key("Right", function()
    if (S.kind or "") ~= "confirm" then return false end
    yes = false
    return true
end)

kn9t.on_key("y", function()
    if (S.kind or "") ~= "confirm" then return false end
    return submit({ value = true })
end)

kn9t.on_key("n", function()
    if (S.kind or "") ~= "confirm" then return false end
    return submit({ value = false })
end)

kn9t.on_key("Enter", function() return submit_current() end)

-- Quick pick: a digit selects and submits the matching option.
local function bind_digit(i)
    kn9t.on_key(tostring(i), function()
        if is_text() or i > #(S.options or {}) then return false end
        cursor = i
        return submit_current()
    end)
end
for i = 1, 9 do bind_digit(i) end

kn9t.on_text(function(ch)
    if not is_text() then return false end
    text = text .. ch
    return true
end)

function render(state)
    state = state or {}
    S = state

    -- Reset the local view when the question changes. The host re-renders every
    -- frame, so this must key off the question, not the call.
    local key = tostring(state.index or 0) .. "|" .. tostring(state.question or "")
    if key ~= seen then
        seen = key
        cursor = 1
        toggles = {}
        text = (type(state.default) == "string") and state.default or ""
        yes = state.default ~= false
    end

    local rows = {}
    local kind = state.kind or "text"

    if state.total and state.total > 1 then
        table.insert(rows, {
            type = "text",
            size = { fixed = 1 },
            fg = "#89b4fa",
            content = string.format("question %d/%d", state.index or 1, state.total),
        })
    end

    -- Reserved height, so the question is never squeezed out of the slot.
    table.insert(rows, {
        type = "text",
        wrap = true,
        size = { fixed = 2 },
        fg = "#cdd6f4",
        content = state.question or "(waiting)",
    })

    local n = #(state.options or {})
    local hint
    if kind == "choice" or kind == "multi" then
        local items = {}
        for i = 1, n do
            local label = option_label(i)
            if kind == "multi" then
                label = (toggles[i] and "[x] " or "[ ] ") .. label
            end
            items[i] = label
        end
        table.insert(rows, {
            type = "list",
            items = items,
            -- cursor is 1-based (indexes options); the list's selected is 0-based.
            selected = cursor - 1,
            size = { fixed = math.min(n, 8) },
        })
        hint = (kind == "multi")
            and "up/down move - Space toggle - Enter submit - Esc cancel"
            or "up/down move - Enter submit - Esc cancel"
    elseif kind == "confirm" then
        table.insert(rows, {
            type = "list",
            items = { "Yes", "No" },
            selected = yes and 0 or 1,
            size = { fixed = 2 },
        })
        hint = "up/down move - Enter submit - Esc cancel"
    else
        table.insert(rows, {
            type = "text",
            size = { fixed = 1 },
            fg = "#f9e2af",
            content = "> " .. text .. "_",
        })
        hint = "type your answer - Enter submit - Esc cancel"
    end

    table.insert(rows, {
        type = "text",
        size = { fixed = 1 },
        fg = "#585b70",
        content = hint,
    })

    return { type = "split", direction = "vertical", children = rows }
end
`;

/**
 * Rows the view needs, so the reserved slot matches its content: +2 border,
 * +1 footer, +2 question, +1 progress for a sequence.
 */
function rowsFor(kind: string, options: QuestionOption[] | undefined, progress: boolean): number {
  const base = 2 + 1 + 2 + (progress ? 1 : 0);
  if (kind === "choice" || kind === "multi") {
    return base + Math.min(options?.length ?? 0, 8);
  }
  if (kind === "confirm") return base + 2;
  return base + 1;
}

/**
 * Register the view for one question. Re-sent per question (not per session)
 * because `uiDone` drops it: an idle session keeps no panel.
 */
function registerUi(session: string, rows: number): void {
  if (!session) return;
  try {
    hostRequest("ui_register_lua", {
      session,
      source: UI_LUA,
      placement: "bottom",
      title: "question",
      rows,
    });
  } catch {
    // Ignore: display is not worth failing a question over.
  }
}

/**
 * Push display state. Best-effort: a UI failure must never fail the tool, since
 * the answer matters more than its presentation.
 */
function setUiState(session: string, state: Record<string, unknown>): void {
  try {
    hostRequest("ui_set_state", { session, state });
  } catch {
    // Ignore: display is not worth failing a question over.
  }
}

/** Describe the current question to the UI. */
function uiAsking(
  session: string,
  kind: string,
  question: string,
  options?: QuestionOption[],
  index?: number,
  total?: number,
  def?: unknown,
): void {
  registerUi(session, rowsFor(kind, options, (total ?? 1) > 1));
  setUiState(session, {
    kind,
    question,
    options: options?.map((o) => ({
      label: o.label,
      value: o.value ?? o.label,
      description: o.description,
    })),
    index,
    total,
    default: def,
  });
}

/** Drop the view once the question resolves, so nothing lingers when idle. */
function uiDone(session: string): void {
  try {
    hostRequest("ui_clear", { session });
  } catch {
    // Ignore: display is not worth failing a question over.
  }
}

interface SeqCtx {
  index?: number;
  total?: number;
}

function executeQuestion(spec: QuestionSpec, session: string, ctx: SeqCtx = {}): ToolResult {
  switch (spec.type) {
    case "text":
      return executeText(spec, session, ctx);
    case "choice":
      return executeChoice(spec, session, ctx);
    case "multi":
      return executeMulti(spec, session, ctx);
    case "confirm":
      return executeConfirm(spec, session, ctx);
    case "sequence":
      return executeSequence(spec, session);
    default:
      return err(`Unknown question type: ${(spec as { type: string }).type}`);
  }
}

function executeText(q: TextQuestion, session: string, ctx: SeqCtx = {}): ToolResult {
  const payload: Record<string, unknown> = {
    type: "text",
    question: q.question,
  };
  if (q.header) payload.header = q.header;
  if (q.placeholder) payload.placeholder = q.placeholder;
  if (q.default) payload.default = q.default;

  uiAsking(session, "text", q.question, undefined, ctx.index, ctx.total, q.default);
  const r = hostRequest("interaction_request", { session, payload });
  uiDone(session);
  if (!r.ok) return err(`interaction_request: ${r.error}`);

  const answer = r.result?.payload as Record<string, unknown> | undefined;
  if (answer?.cancelled) return ok("User cancelled.");
  
  const value = answer?.value ?? "";
  return ok(`User answered: ${value}`);
}

function executeChoice(q: ChoiceQuestion, session: string, ctx: SeqCtx = {}): ToolResult {
  const payload: Record<string, unknown> = {
    type: "choice",
    question: q.question,
    options: q.options,
  };
  if (q.header) payload.header = q.header;
  if (q.allow_custom) payload.allow_custom = true;

  uiAsking(session, "choice", q.question, q.options, ctx.index, ctx.total);
  const r = hostRequest("interaction_request", { session, payload });
  uiDone(session);
  if (!r.ok) return err(`interaction_request: ${r.error}`);

  const answer = r.result?.payload as Record<string, unknown> | undefined;
  if (answer?.cancelled) return ok("User cancelled.");

  const selected = answer?.value;
  if (typeof selected === "string") {
    return ok(`User selected: ${selected}`);
  }
  return ok(`User selected: ${JSON.stringify(selected)}`);
}

function executeMulti(q: MultiQuestion, session: string, ctx: SeqCtx = {}): ToolResult {
  const payload: Record<string, unknown> = {
    type: "multi",
    question: q.question,
    options: q.options,
  };
  if (q.header) payload.header = q.header;
  if (q.min !== undefined) payload.min = q.min;
  if (q.max !== undefined) payload.max = q.max;

  uiAsking(session, "multi", q.question, q.options, ctx.index, ctx.total);
  const r = hostRequest("interaction_request", { session, payload });
  uiDone(session);
  if (!r.ok) return err(`interaction_request: ${r.error}`);

  const answer = r.result?.payload as Record<string, unknown> | undefined;
  if (answer?.cancelled) return ok("User cancelled.");

  const selected = answer?.value;
  if (Array.isArray(selected)) {
    return ok(`User selected: ${selected.join(", ")}`);
  }
  return ok(`User selected: ${JSON.stringify(selected)}`);
}

function executeConfirm(q: ConfirmQuestion, session: string, ctx: SeqCtx = {}): ToolResult {
  const payload: Record<string, unknown> = {
    type: "confirm",
    question: q.question,
  };
  if (q.header) payload.header = q.header;
  if (q.default !== undefined) payload.default = q.default;

  uiAsking(session, "confirm", q.question, undefined, ctx.index, ctx.total, q.default);
  const r = hostRequest("interaction_request", { session, payload });
  uiDone(session);
  if (!r.ok) return err(`interaction_request: ${r.error}`);

  const answer = r.result?.payload as Record<string, unknown> | undefined;
  if (answer?.cancelled) return ok("User cancelled.");

  const confirmed = answer?.value === true || answer?.value === "yes";
  return ok(confirmed ? "User confirmed: Yes" : "User declined: No");
}

function executeSequence(q: SequenceQuestion, session: string): ToolResult {
  const results: string[] = [];
  const total = q.questions.length;

  for (let i = 0; i < total; i++) {
    const subQ = q.questions[i];
    // Renders itself; the context makes its header report "3/5".
    const result = executeQuestion(subQ, session, { index: i + 1, total });

    if (result.is_error) {
      return result; // Propagate error
    }
    
    const text = result.content[0]?.text ?? "";
    if (text.includes("cancelled")) {
      return ok(`Sequence cancelled at question ${i + 1}/${total}`);
    }
    
    results.push(`Q${i + 1}: ${text}`);
  }
  
  return ok(`Sequence completed:\n${results.join("\n")}`);
}

// ── Legacy support ───────────────────────────────────────────────────────────

function executeLegacy(args: Record<string, unknown>, session: string): ToolResult {
  // Support old format: {question: string, choices?: string[]}
  const question = args.question as string;
  const choices = args.choices as string[] | undefined;
  
  if (choices && choices.length > 0) {
    // Convert to choice question
    return executeChoice({
      type: "choice",
      question,
      options: choices.map(c => ({ label: c })),
      allow_custom: true,
    }, session);
  }
  
  // Default to text question
  return executeText({
    type: "text",
    question,
    placeholder: args.placeholder as string | undefined,
  }, session);
}

// ── Hook handler ─────────────────────────────────────────────────────────────

function handleHook(id: number, payload: Record<string, unknown>): void {
  const name = String(payload.tool ?? "");
  const args = (payload.args as Record<string, unknown>) ?? {};
  const session = String(payload.session ?? "");

  if (name !== "ask_user") {
    writeMsg({
      t: "result",
      id,
      content: [{ type: "text", text: `kn9t-ask-user: unknown tool ${name}` }],
      is_error: true,
    });
    return;
  }

  // Validate required field
  const question = args.question;
  if (typeof question !== "string" && !args.questions) {
    writeMsg({
      t: "result",
      id,
      content: [{ type: "text", text: 'ask_user requires "question" or "questions"' }],
      is_error: true,
    });
    return;
  }

  let result: ToolResult;

  // Check if it's the new format with explicit type
  if (args.type) {
    result = executeQuestion(args as unknown as QuestionSpec, session);
  } else if (args.questions && Array.isArray(args.questions)) {
    // Sequence shorthand
    result = executeSequence({
      type: "sequence",
      questions: args.questions as QuestionSpec[],
    }, session);
  } else {
    // Legacy format
    result = executeLegacy(args, session);
  }

  writeMsg({ t: "result", id, ...result });
}

// ── Main ─────────────────────────────────────────────────────────────────────

function main(): void {
  const hello = reader.readLine();
  if (hello === null) process.exit(1);
  
  const h = JSON.parse(hello) as { t?: string };
  if (h.t !== "hello") {
    console.error("expected hello");
    process.exit(1);
  }

  writeMsg({
    t: "hello",
    name: "kn9t-ask-user",
    capabilities: ["host_api"],
    tools: [
      {
        name: "ask_user",
        description: `Ask the human a question and wait for their reply.

Supports multiple question types:
- text: Free-form text input (default)
- choice: Single selection from options  
- multi: Multiple selection from options
- confirm: Yes/No confirmation
- sequence: Multiple questions in order

Examples:
  Simple text: {"question": "What's your name?"}
  
  Choice: {"type": "choice", "question": "Pick one", "options": [
    {"label": "Option A", "description": "First option"},
    {"label": "Option B", "description": "Second option"}
  ]}
  
  Multi-select: {"type": "multi", "question": "Select all that apply", "options": [...]}
  
  Confirm: {"type": "confirm", "question": "Delete these files?"}
  
  Sequence: {"questions": [
    {"type": "text", "question": "Project name?"},
    {"type": "choice", "question": "Language?", "options": [...]}
  ]}

Use for genuine ambiguity, not to avoid deciding.`,
        schema: {
          type: "object",
          properties: {
            type: {
              type: "string",
              enum: ["text", "choice", "multi", "confirm", "sequence"],
              description: "Question type. Default: text",
            },
            question: {
              type: "string",
              description: "The question to ask (required for text/choice/multi/confirm)",
            },
            header: {
              type: "string",
              description: "Short header/title (max 30 chars)",
            },
            options: {
              type: "array",
              items: {
                type: "object",
                properties: {
                  label: { type: "string", description: "Display text (1-5 words)" },
                  value: { type: "string", description: "Return value (default: label)" },
                  description: { type: "string", description: "Explanation of this option" },
                },
                required: ["label"],
              },
              description: "Options for choice/multi questions",
            },
            questions: {
              type: "array",
              description: "Sub-questions for sequence type",
            },
            allow_custom: {
              type: "boolean",
              description: "Allow custom input for choice questions",
            },
            placeholder: {
              type: "string",
              description: "Placeholder text for text input",
            },
            default: {
              description: "Default value",
            },
          },
        },
        parallel_safe: false,
      },
    ],
  });

  while (true) {
    const line = reader.readLine();
    if (line === null) break;
    
    const msg = JSON.parse(line) as { t?: string; id?: number } & Record<string, unknown>;
    if (msg.t === "shutdown") break;
    if (msg.t === "hook" && typeof msg.id === "number") {
      handleHook(msg.id, (msg.payload as Record<string, unknown>) ?? {});
    }
  }
}

try {
  main();
} catch (e) {
  console.error("kn9t-ask-user fatal:", e);
  process.exit(1);
}
