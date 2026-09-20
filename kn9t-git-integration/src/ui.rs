//! Serializes git state to JSON and defines the interactive Lua view sent once
//! via `ui_register_lua`.
//!
//! Unlike the first version of this plugin, the Lua here is not render-only: it
//! binds its own keys and clicks via `kn9t.on_key`/`kn9t.on_click`, so the diff
//! review panel is genuinely owned by this plugin. A user does not have to add
//! anything to `tui.lua` beyond choosing where the view goes:
//!
//! ```lua
//! { type = "plugin", plugin = "kn9t-git-integration" }
//! ```

use crate::diff::{DiffFile, DiffTarget};
use crate::git::GitState;

/// JSON pushed via `ui_set_state`.
///
/// `repo` is `null` (not an empty object) when `cwd` is not a git repository,
/// so the Lua side can render a distinct message instead of an
/// empty-but-misleadingly-"clean" status.
use std::collections::HashMap;

/// Commit diffs: sha -> list of changed files with hunks
pub type CommitDiffs = HashMap<String, Vec<DiffFile>>;

pub fn state_to_json(
    state: Option<&GitState>,
    files: &[DiffFile],
    diff_target: &DiffTarget,
    commit_diffs: &CommitDiffs,
) -> serde_json::Value {
    let repo = match state {
        None => serde_json::Value::Null,
        Some(s) => serde_json::json!({
            "branch": s.branch,
            "ahead": s.ahead,
            "behind": s.behind,
            "changes": s.changes.iter().map(|c| serde_json::json!({
                "status": c.status,
                "path": c.path,
            })).collect::<Vec<_>>(),
            "recent": s.recent.iter().map(|l| serde_json::json!({
                "sha": l.sha,
                "subject": l.subject,
                "author": l.author,
                "date": l.date,
                "refs": l.refs,
                "graph": l.graph,
            })).collect::<Vec<_>>(),
            "refs": s.refs.iter().map(|r| serde_json::json!({
                "name": r.name,
                "kind": r.kind.tag(),
                "is_current": r.is_current,
            })).collect::<Vec<_>>(),
            "stashes": s.stashes,
        }),
    };

    serde_json::json!({
        "repo": repo,
        "diff": files.iter().map(diff_file_to_json).collect::<Vec<_>>(),
        "diff_target": diff_target.label(),
        "commit_diffs": commit_diffs.iter().map(|(sha, cfiles)| {
            serde_json::json!({
                "sha": sha,
                "files": cfiles.iter().map(diff_file_to_json).collect::<Vec<_>>(),
            })
        }).collect::<Vec<_>>(),
    })
}

fn diff_file_to_json(f: &DiffFile) -> serde_json::Value {
    serde_json::json!({
        "path": f.path,
        "status": f.status.tag(),
        "additions": f.additions,
        "deletions": f.deletions,
        "hunks": f.hunks.iter().map(|h| serde_json::json!({
            "header": h.header,
            "old_start": h.old_start,
            "new_start": h.new_start,
            "lines": h.lines.iter().map(|l| serde_json::json!({
                "kind": l.kind.tag(),
                "text": l.text,
                // Serialized per line rather than derived Lua-side from the
                // hunk start plus an index: removed lines occupy no new-file
                // line, so index arithmetic drifts after the first deletion.
                "new_lineno": l.new_lineno,
                "old_lineno": l.old_lineno,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

/// The plugin's Lua view: `render(state)` plus its own key and click bindings.
///
/// Sent once via `ui_register_lua` (256 KB cap — this is well under it).
///
/// # Why the state split
///
/// `state` (pushed from Rust) is data about the repository. `V` (Lua-local) is
/// view state: cursor, mode, comments. They are deliberately separate — a poll
/// arriving every few seconds must not reset the cursor the user just moved, so
/// nothing in `V` is ever derived from a fresh `state` except by clamping.
pub const LUA_SOURCE: &str = r##"
-- View state. Survives `ui_set_state`, which only replaces repo data.
V = {
  mode = "status",   -- "status" | "diff" | "graph" | "commit"
  split = false,     -- side-by-side vs unified
  tree = true,       -- show the file list
  height = 20,       -- last known viewport height
  comments = {},     -- { {path=, line=, text=, commit=nil|sha} }
  typing = nil,      -- in-progress comment text
  
  -- Diff navigation (shared between "diff" and "commit" modes)
  diff_file = 1,     -- 1-based index into files
  diff_cursor = 1,   -- 1-based index into flattened lines
  diff_scroll = 0,
  diff_sha = nil,    -- nil for working tree, sha for commit
  
  -- Graph navigation & pagination
  graph_cursor = 1,
  graph_scroll = 0,
  graph_page = 1,
  graph_page_size = 100,
  graph_search = nil,
  graph_searching = false,
  
  -- Graph filters
  show_local = true,
  show_remote = true,
  show_tags = false,
  show_stash = false,
  
  -- Refs panel
  refs_cursor = 1,
  show_refs = false,
}

local LAST = nil

-- ── Data accessors ───────────────────────────────────────────────────────────

local function working_files()
  if LAST == nil or LAST.diff == nil then return {} end
  return LAST.diff
end

local function commit_files_for(sha)
  if LAST == nil or LAST.commit_diffs == nil or sha == nil then return {} end
  for _, cd in ipairs(LAST.commit_diffs) do
    if cd.sha == sha then return cd.files or {} end
  end
  return {}
end

-- Returns the files for the current diff context (working tree or commit)
local function diff_files()
  if V.diff_sha then
    return commit_files_for(V.diff_sha)
  end
  return working_files()
end

local function commits()
  if LAST == nil or LAST.repo == nil or LAST.repo.recent == nil then return {} end
  local out = {}
  for _, l in ipairs(LAST.repo.recent) do
    if l.sha and l.sha ~= "" then table.insert(out, l) end
  end
  return out
end

local function filtered_commits()
  local all = commits()
  local out = {}
  
  for _, c in ipairs(all) do
    if V.graph_search and V.graph_search ~= "" then
      local query = string.lower(V.graph_search)
      local match = string.find(string.lower(c.sha or ""), query, 1, true)
        or string.find(string.lower(c.subject or ""), query, 1, true)
        or string.find(string.lower(c.author or ""), query, 1, true)
      if not match then goto continue end
    end
    
    local has_visible_ref = false
    if not c.refs or #c.refs == 0 then
      has_visible_ref = true
    else
      for _, ref in ipairs(c.refs) do
        local is_local = not string.find(ref, "origin/")
        local is_remote = string.find(ref, "origin/") ~= nil
        local is_tag = string.find(ref, "tag:") ~= nil
        if is_local and V.show_local then has_visible_ref = true end
        if is_remote and V.show_remote then has_visible_ref = true end
        if is_tag and V.show_tags then has_visible_ref = true end
      end
    end
    
    if has_visible_ref then table.insert(out, c) end
    ::continue::
  end
  return out
end

local function paged_commits()
  local filtered = filtered_commits()
  local start_idx = (V.graph_page - 1) * V.graph_page_size + 1
  local end_idx = start_idx + V.graph_page_size - 1
  local out = {}
  for i = start_idx, math.min(end_idx, #filtered) do
    table.insert(out, filtered[i])
  end
  return out, #filtered
end

local function total_pages()
  local filtered = filtered_commits()
  return math.max(1, math.ceil(#filtered / V.graph_page_size))
end

-- ── Unified diff navigation ──────────────────────────────────────────────────

local function cur_file()
  local f = diff_files()
  if #f == 0 then return nil end
  if V.diff_file > #f then V.diff_file = #f end
  if V.diff_file < 1 then V.diff_file = 1 end
  return f[V.diff_file]
end

local function rows(file)
  local out = {}
  if file == nil then return out end
  for _, h in ipairs(file.hunks or {}) do
    table.insert(out, { hunk = true, text = h.header })
    for _, l in ipairs(h.lines or {}) do
      table.insert(out, {
        kind = l.kind,
        text = l.text,
        new_lineno = l.new_lineno,
        old_lineno = l.old_lineno,
      })
    end
  end
  return out
end

local function clamp_diff_cursor(n)
  if V.diff_cursor < 1 then V.diff_cursor = 1 end
  if n > 0 and V.diff_cursor > n then V.diff_cursor = n end
  local view = math.max(1, V.height - 2)
  if V.diff_cursor <= V.diff_scroll then V.diff_scroll = V.diff_cursor - 1 end
  if V.diff_cursor > V.diff_scroll + view then V.diff_scroll = V.diff_cursor - view end
  if V.diff_scroll < 0 then V.diff_scroll = 0 end
end

local function clamp_graph_cursor()
  local paged, total = paged_commits()
  if V.graph_cursor < 1 then V.graph_cursor = 1 end
  if #paged > 0 and V.graph_cursor > #paged then V.graph_cursor = #paged end
  local view = math.max(1, V.height - 3)
  if V.graph_cursor <= V.graph_scroll then V.graph_scroll = V.graph_cursor - 1 end
  if V.graph_cursor > V.graph_scroll + view then V.graph_scroll = V.graph_cursor - view end
  if V.graph_scroll < 0 then V.graph_scroll = 0 end
end

local function comment_at(path, line, sha)
  for _, c in ipairs(V.comments) do
    if c.path == path and c.line == line and c.commit == sha then
      return c.text
    end
  end
  return nil
end

-- ── input ────────────────────────────────────────────────────────────────────

-- All bindings go through `bind`, which owns one rule: while a comment is being
-- composed, a single printable key is text, not a command. Registering the
-- semantic keys and then a separate printable loop would clobber them (`d`, `u`,
-- `c`, `j` are all both), so there is exactly one handler per key.
local BOUND = {}

local function bind(key, fn)
  BOUND[key] = true
  kn9t.on_key(key, function()
    -- Handle search input in graph mode
    if V.graph_searching and #key == 1 then
      V.graph_search = V.graph_search .. key
      V.graph_page = 1  -- Reset to first page on new search
      V.graph_cursor = 1
      V.graph_scroll = 0
      return true
    end
    -- Handle comment input
    if V.typing ~= nil and #key == 1 then
      V.typing = V.typing .. key
      return true
    end
    return fn()
  end)
end

-- Helper: check if we're in a diff-viewing mode (diff or commit)
local function in_diff_mode()
  return V.mode == "diff" or V.mode == "commit"
end

bind("j", function()
  if V.mode == "graph" then
    V.graph_cursor = V.graph_cursor + 1
    clamp_graph_cursor()
  elseif in_diff_mode() then
    V.diff_cursor = V.diff_cursor + 1
    clamp_diff_cursor(#rows(cur_file()))
  end
end)
bind("k", function()
  if V.mode == "graph" then
    V.graph_cursor = V.graph_cursor - 1
    clamp_graph_cursor()
  elseif in_diff_mode() then
    V.diff_cursor = V.diff_cursor - 1
    clamp_diff_cursor(#rows(cur_file()))
  end
end)
bind("Down", function()
  if V.mode == "graph" then
    V.graph_cursor = V.graph_cursor + 1
    clamp_graph_cursor()
  elseif in_diff_mode() then
    V.diff_cursor = V.diff_cursor + 1
    clamp_diff_cursor(#rows(cur_file()))
  end
end)
bind("Up", function()
  if V.mode == "graph" then
    V.graph_cursor = V.graph_cursor - 1
    clamp_graph_cursor()
  elseif in_diff_mode() then
    V.diff_cursor = V.diff_cursor - 1
    clamp_diff_cursor(#rows(cur_file()))
  end
end)

bind("PageDown", function()
  local step = math.max(1, V.height - 2)
  if V.mode == "graph" then
    V.graph_cursor = V.graph_cursor + step
    clamp_graph_cursor()
  elseif in_diff_mode() then
    V.diff_cursor = V.diff_cursor + step
    clamp_diff_cursor(#rows(cur_file()))
  end
end)
bind("PageUp", function()
  local step = math.max(1, V.height - 2)
  if V.mode == "graph" then
    V.graph_cursor = V.graph_cursor - step
    clamp_graph_cursor()
  elseif in_diff_mode() then
    V.diff_cursor = V.diff_cursor - step
    clamp_diff_cursor(#rows(cur_file()))
  end
end)

bind("n", function()
  if in_diff_mode() then
    local f = diff_files()
    if V.diff_file < #f then
      V.diff_file = V.diff_file + 1
      V.diff_cursor = 1
      V.diff_scroll = 0
    end
  end
end)
bind("p", function()
  if in_diff_mode() then
    if V.diff_file > 1 then
      V.diff_file = V.diff_file - 1
      V.diff_cursor = 1
      V.diff_scroll = 0
    end
  end
end)

bind("u", function() V.split = not V.split end)
bind("b", function() V.tree = not V.tree end)

bind("d", function()
  if V.mode == "diff" then
    V.mode = "status"
  elseif V.mode == "commit" then
    V.mode = "graph"
    kn9t.notify({ event = "clear_commit_diff" })
  else
    V.mode = "diff"
    V.diff_sha = nil
    V.diff_file = 1
    V.diff_cursor = 1
    V.diff_scroll = 0
  end
end)

bind("g", function()
  if V.mode == "graph" then
    V.mode = "status"
  elseif V.mode == "commit" then
    V.mode = "graph"
    kn9t.notify({ event = "clear_commit_diff" })
  else
    V.mode = "graph"
    V.graph_cursor = 1
    V.graph_scroll = 0
  end
end)

bind("Backspace", function()
  if V.graph_searching then
    V.graph_search = string.sub(V.graph_search or "", 1, -2)
    return true
  end
  if V.typing ~= nil then
    V.typing = string.sub(V.typing, 1, -2)
    return true
  end
  if V.mode == "commit" then
    V.mode = "graph"
    V.diff_sha = nil
    kn9t.notify({ event = "clear_commit_diff" })
    return true
  end
  return true
end)

-- Toggle refs panel
bind("r", function()
  V.show_refs = not V.show_refs
end)

-- Filter toggles (in graph mode)
bind("1", function()
  if V.mode == "graph" or V.mode == "status" then
    V.show_local = not V.show_local
  end
end)
bind("2", function()
  if V.mode == "graph" or V.mode == "status" then
    V.show_remote = not V.show_remote
  end
end)
bind("3", function()
  if V.mode == "graph" or V.mode == "status" then
    V.show_tags = not V.show_tags
  end
end)
bind("4", function()
  if V.mode == "graph" or V.mode == "status" then
    V.show_stash = not V.show_stash
  end
end)

-- Pagination: [ prev page, ] next page
bind("[", function()
  if V.mode == "graph" then
    if V.graph_page > 1 then
      V.graph_page = V.graph_page - 1
      V.graph_cursor = 1
      V.graph_scroll = 0
    end
    return true
  end
  if in_diff_mode() then
    local r = rows(cur_file())
    for i = V.diff_cursor - 1, 1, -1 do
      if r[i].hunk then V.diff_cursor = i; clamp_diff_cursor(#r); return true end
    end
    if V.diff_file > 1 then
      V.diff_file = V.diff_file - 1
      local pr = rows(cur_file())
      V.diff_cursor = #pr > 0 and #pr or 1
      clamp_diff_cursor(#pr)
    end
  end
  return true
end)

bind("]", function()
  if V.mode == "graph" then
    if V.graph_page < total_pages() then
      V.graph_page = V.graph_page + 1
      V.graph_cursor = 1
      V.graph_scroll = 0
    end
    return true
  end
  if in_diff_mode() then
    local r = rows(cur_file())
    for i = V.diff_cursor + 1, #r do
      if r[i].hunk then V.diff_cursor = i; clamp_diff_cursor(#r); return true end
    end
    local f = diff_files()
    if V.diff_file < #f then
      V.diff_file = V.diff_file + 1
      V.diff_cursor = 1
      V.diff_scroll = 0
    end
  end
  return true
end)

bind("C-f", function()
  if V.mode == "graph" then
    V.graph_searching = true
    V.graph_search = ""
  end
  return true
end)

bind("c", function()
  if in_diff_mode() then V.typing = "" end
  return true
end)

bind("Enter", function()
  if V.graph_searching then
    V.graph_searching = false
    return true
  end
  
  -- Handle comment submission (unified for diff and commit modes)
  if V.typing ~= nil and in_diff_mode() then
    local f = cur_file()
    local r = rows(f)
    local row = r[V.diff_cursor]
    if f ~= nil and row ~= nil and V.typing ~= "" then
      local line = row.new_lineno or row.old_lineno
      if line ~= nil then
        table.insert(V.comments, {
          path = f.path,
          line = line,
          text = V.typing,
          commit = V.diff_sha,
        })
      end
    end
    V.typing = nil
    return true
  end
  
  -- Enter in graph mode: view commit
  if V.mode == "graph" then
    local paged, _ = paged_commits()
    if #paged > 0 and V.graph_cursor <= #paged then
      local commit = paged[V.graph_cursor]
      if commit and commit.sha and commit.sha ~= "" then
        V.diff_sha = commit.sha
        V.diff_file = 1
        V.diff_cursor = 1
        V.diff_scroll = 0
        V.mode = "commit"
        kn9t.notify({ event = "request_commit_diff", sha = commit.sha })
      end
    end
    return true
  end
  
  return true
end)

-- Ctrl+Q: universal back/cancel - works even while typing
kn9t.on_key("C-q", function()
  if V.graph_searching then
    V.graph_searching = false
    V.graph_search = nil
    V.graph_page = 1
    V.graph_cursor = 1
    V.graph_scroll = 0
    return true
  end
  if V.typing ~= nil then
    V.typing = nil
    return true
  end
  if V.mode == "commit" then
    V.mode = "graph"
    V.diff_sha = nil
    kn9t.notify({ event = "clear_commit_diff" })
    return true
  end
  if V.mode == "diff" or V.mode == "graph" then
    V.mode = "status"
    return true
  end
  return true
end)

-- Hand the collected review to the prompt. This is the one host mutation a
-- plugin view can request, and the workflow the whole panel exists for.
bind("C-s", function()
  if #V.comments == 0 then return false end
  local parts = {}
  for _, c in ipairs(V.comments) do
    if c.commit then
      -- Commit comment format: [commit:sha file:path line:N] comment
      table.insert(parts, "[commit:" .. c.commit .. " " .. c.path .. ":" .. c.line .. "] " .. c.text)
    else
      -- Working tree comment format: [path:line] comment
      table.insert(parts, "[" .. c.path .. ":" .. c.line .. "] " .. c.text)
    end
  end
  kn9t.insert_input(table.concat(parts, "\n"))
  V.comments = {}
end)

-- Remaining printable characters: always consume them when focused to prevent
-- typing in the user input. When composing a comment, append to V.typing.
local PRINTABLE = "abcdefghijklmnopqrstuvwxyz0123456789.,:;!?/()<>-_=+*#@'\"`~$%^&|\\"
for i = 1, #PRINTABLE do
  local ch = string.sub(PRINTABLE, i, i)
  if not BOUND[ch] then
    bind(ch, function() return true end)
  end
end

-- Uppercase letters: Windows sends "S-A", Unix sends "A" (no shift modifier)
-- Register both forms to handle cross-platform
for i = 1, 26 do
  local upper = string.char(64 + i)  -- A=65
  local handler = function()
    if V.graph_searching then
      V.graph_search = V.graph_search .. upper
      V.graph_page = 1
      V.graph_cursor = 1
      V.graph_scroll = 0
      return true
    end
    if V.typing ~= nil then
      V.typing = V.typing .. upper
      return true
    end
    return true  -- Consume to prevent input leak
  end
  kn9t.on_key("S-" .. upper, handler)  -- Windows: Shift+A = "S-A"
  kn9t.on_key(upper, handler)          -- Unix: Shift+A = "A"
end

-- Space is sent as "Space" by the TUI, not " ". Always consume.
kn9t.on_key("Space", function()
  if V.typing ~= nil then
    V.typing = V.typing .. " "
  end
  return true  -- Always consume
end)

-- Clicking a file row selects it
kn9t.on_click("files", function(x, y)
  local f = diff_files()
  local idx = y + 1
  if idx >= 1 and idx <= #f then
    V.diff_file = idx
    V.diff_cursor = 1
    V.diff_scroll = 0
  end
end)

-- Clicking a graph row selects/enters the commit
kn9t.on_click("graph", function(x, y)
  if V.mode ~= "graph" then return false end
  local paged, _ = paged_commits()
  local idx = y + 1
  if idx >= 1 and idx <= #paged then
    V.graph_cursor = idx
    local commit = paged[idx]
    if commit and commit.sha and commit.sha ~= "" then
      V.diff_sha = commit.sha
      V.diff_file = 1
      V.diff_cursor = 1
      V.diff_scroll = 0
      V.mode = "commit"
      kn9t.notify({ event = "request_commit_diff", sha = commit.sha })
    end
  end
end)

kn9t.on_click("body", function(x, y)
  if not in_diff_mode() then return false end
  local target = V.diff_scroll + y + 1
  local n = #rows(cur_file())
  if target >= 1 and target <= n then
    if target == V.diff_cursor and V.typing == nil then
      V.typing = ""
    else
      V.diff_cursor = target
      clamp_diff_cursor(n)
    end
  end
end)

-- ── render ───────────────────────────────────────────────────────────────────

local function line_style(kind)
  if kind == "add" then return "green" end
  if kind == "del" then return "lightred" end
  return "gray"
end

local function line_bg(kind)
  if kind == "add" then return "#1a2e1a" end
  if kind == "del" then return "#2e1a1a" end
  return nil
end

-- Graph character styling
local function graph_char_color(ch)
  if ch == "*" then return "yellow" end
  if ch == "|" then return "blue" end
  if ch == "/" or ch == "\\" then return "magenta" end
  return "darkgray"
end

-- Render colored graph prefix
local function graph_spans(graph_str)
  local spans = {}
  for i = 1, #graph_str do
    local ch = string.sub(graph_str, i, i)
    table.insert(spans, { text = ch, fg = graph_char_color(ch) })
  end
  return spans
end

-- Ref badge color
local function ref_color(ref_str)
  if string.find(ref_str, "HEAD") then return "yellow" end
  if string.find(ref_str, "origin/") then return "lightred" end
  if string.find(ref_str, "tag:") then return "cyan" end
  return "green"
end

-- Refs sidebar
local function refs_panel(repo)
  local items = {}
  for i, r in ipairs(repo.refs or {}) do
    local dominated = false
    if r.kind == "local" and not V.show_local then dominated = true end
    if r.kind == "remote" and not V.show_remote then dominated = true end
    if r.kind == "tag" and not V.show_tags then dominated = true end
    if not dominated then
      local col = "gray"
      if r.kind == "local" then col = r.is_current and "green" or "white" end
      if r.kind == "remote" then col = "lightred" end
      if r.kind == "tag" then col = "cyan" end
      table.insert(items, { spans = {
        { text = "  ", fg = col },
        { text = r.name, fg = col, bold = r.is_current },
      }})
    end
  end
  if V.show_stash then
    for _, s in ipairs(repo.stashes or {}) do
      table.insert(items, { spans = {
        { text = "  ", fg = "magenta" },
        { text = s, fg = "magenta" },
      }})
    end
  end
  return {
    type = "list",
    id = "refs",
    items = items,
    size = { fixed = 28 },
  }
end

local function status_view(repo)
  local out = {}
  local branch = repo.branch or "?"
  local ab = ""
  if (repo.ahead or 0) > 0 then ab = ab .. " ↑" .. repo.ahead end
  if (repo.behind or 0) > 0 then ab = ab .. " ↓" .. repo.behind end
  table.insert(out, { type = "text", content = " " .. branch .. ab, fg = "cyan", bold = true,
                      size = { fixed = 1 }, wrap = false })

  local changes = repo.changes or {}
  if #changes == 0 then
    table.insert(out, { type = "text", content = "  (clean)", fg = "darkgray",
                        size = { fixed = 1 }, wrap = false })
  else
    table.insert(out, { type = "text", content = string.format("  %d change(s)", #changes), 
                        fg = "yellow", size = { fixed = 1 }, wrap = false })
    for _, c in ipairs(changes) do
      local col = "yellow"
      if c.status == "?" then col = "green"
      elseif c.status == "D" then col = "lightred"
      elseif c.status == "A" then col = "green" end
      table.insert(out, { type = "text", content = "  " .. c.status .. " " .. c.path, fg = col,
                          size = { fixed = 1 }, wrap = false })
    end
  end

  table.insert(out, { type = "text", content = "", size = { fixed = 1 } })
  
  -- Filter status bar
  local filter_spans = {
    { text = " Filters: ", fg = "darkgray" },
    { text = "[1]", fg = "cyan" },
    { text = V.show_local and "Local " or "local ", fg = V.show_local and "green" or "darkgray" },
    { text = "[2]", fg = "cyan" },
    { text = V.show_remote and "Remote " or "remote ", fg = V.show_remote and "lightred" or "darkgray" },
    { text = "[3]", fg = "cyan" },
    { text = V.show_tags and "Tags " or "tags ", fg = V.show_tags and "yellow" or "darkgray" },
    { text = "[4]", fg = "cyan" },
    { text = V.show_stash and "Stash" or "stash", fg = V.show_stash and "magenta" or "darkgray" },
  }
  table.insert(out, { type = "text", spans = filter_spans, size = { fixed = 1 }, wrap = false })
  
  table.insert(out, { type = "text", content = "", size = { fixed = 1 } })
  table.insert(out, { type = "text", content = " Recent commits:", fg = "darkgray",
                      size = { fixed = 1 }, wrap = false })
  
  -- Show graph preview (recent commits with graph)
  for i, l in ipairs(repo.recent or {}) do
    if i > 8 then break end
    if l.sha == "" then
      -- Graph-only line (merge connector)
      table.insert(out, { type = "text", spans = graph_spans("  " .. l.graph), 
                          size = { fixed = 1 }, wrap = false })
    else
      local spans = { { text = "  ", fg = "darkgray" } }
      for _, s in ipairs(graph_spans(l.graph)) do table.insert(spans, s) end
      table.insert(spans, { text = l.sha .. " ", fg = "yellow" })
      table.insert(spans, { text = l.subject, fg = "white" })
      for _, ref in ipairs(l.refs or {}) do
        if ref ~= "" then
          table.insert(spans, { text = " (" .. ref .. ")", fg = ref_color(ref) })
        end
      end
      table.insert(out, { type = "text", spans = spans, size = { fixed = 1 }, wrap = false })
    end
  end

  -- Help bar at bottom
  table.insert(out, { type = "spacer", size = { flex = 1 } })
  table.insert(out, { type = "text", spans = {
    { text = "[d]", fg = "cyan" }, { text = " diff  ", fg = "darkgray" },
    { text = "[g]", fg = "cyan" }, { text = " graph  ", fg = "darkgray" },
    { text = "[r]", fg = "cyan" }, { text = " refs  ", fg = "darkgray" },
    { text = "[Esc]", fg = "cyan" }, { text = " close", fg = "darkgray" },
  }, size = { fixed = 1 }, wrap = false })

  if V.show_refs then
    return {
      type = "split", direction = "horizontal",
      children = {
        refs_panel(repo),
        { type = "split", direction = "vertical", children = out },
      },
    }
  end
  return { type = "split", direction = "vertical", children = out }
end

-- Full graph view with pagination and search
local function graph_view(repo)
  local items = {}
  local paged, total_count = paged_commits()
  local pages = total_pages()
  
  for i, l in ipairs(paged) do
    local mark = (i == V.graph_cursor) and ">" or " "
    
    if l.sha == "" then
      local spans = { { text = mark, fg = "yellow" } }
      for _, s in ipairs(graph_spans(l.graph)) do table.insert(spans, s) end
      table.insert(items, { spans = spans })
    else
      local spans = { { text = mark, fg = "yellow", bold = i == V.graph_cursor } }
      for _, s in ipairs(graph_spans(l.graph)) do table.insert(spans, s) end
      table.insert(spans, { text = l.sha, fg = "yellow", bold = true })
      table.insert(spans, { text = " " })
      for _, ref in ipairs(l.refs or {}) do
        if ref ~= "" then
          table.insert(spans, { text = "[" .. ref .. "] ", fg = ref_color(ref) })
        end
      end
      table.insert(spans, { text = l.subject, fg = "white" })
      if l.author ~= "" then
        table.insert(spans, { text = " - " .. l.author .. ", " .. l.date, fg = "darkgray" })
      end
      table.insert(items, { spans = spans })
    end
  end
  
  -- List with current selection; TUI handles scrolling automatically
  local graph_list = { type = "list", id = "graph", items = items, selected = V.graph_cursor - 1, offset = V.graph_scroll }
  
  -- Header with search and pagination info
  local header_spans = {
    { text = " Git Graph  ", fg = "cyan", bold = true },
  }
  
  -- Show search query if searching
  if V.graph_searching then
    table.insert(header_spans, { text = "Search: ", fg = "yellow" })
    table.insert(header_spans, { text = V.graph_search .. "_", fg = "white", bold = true })
  elseif V.graph_search and V.graph_search ~= "" then
    table.insert(header_spans, { text = "\"" .. V.graph_search .. "\" ", fg = "yellow" })
    table.insert(header_spans, { text = "(" .. total_count .. " matches) ", fg = "darkgray" })
  end
  
  -- Pagination info
  if pages > 1 then
    table.insert(header_spans, { text = string.format(" Page %d/%d ", V.graph_page, pages), fg = "magenta" })
  end
  
  local footer_spans = {
    { text = "[j/k]", fg = "cyan" }, { text = " nav  ", fg = "darkgray" },
    { text = "[[/]]", fg = "cyan" }, { text = " page  ", fg = "darkgray" },
    { text = "[C-f]", fg = "cyan" }, { text = " search  ", fg = "darkgray" },
    { text = "[Enter]", fg = "cyan" }, { text = " view  ", fg = "darkgray" },
    { text = "[C-q]", fg = "cyan" }, { text = " back", fg = "darkgray" },
  }
  
  local children = {
    { type = "text", spans = header_spans, size = { fixed = 1 }, wrap = false },
    graph_list,
    { type = "text", spans = footer_spans, size = { fixed = 1 }, wrap = false },
  }
  
  if V.show_refs then
    return {
      type = "split", direction = "horizontal",
      children = {
        refs_panel(repo),
        { type = "split", direction = "vertical", children = children },
      },
    }
  end
  return { type = "split", direction = "vertical", children = children }
end

-- ── Unified diff rendering ───────────────────────────────────────────────────

local function file_list()
  local items = {}
  local f = diff_files()
  for i, file in ipairs(f) do
    local col = "yellow"
    if file.status == "A" then col = "green"
    elseif file.status == "D" then col = "lightred" end
    local selected = (i == V.diff_file)
    table.insert(items, { spans = {
      { text = selected and ">" or " ", fg = "yellow" },
      { text = file.status .. " ", fg = col },
      { text = file.path .. " " },
      { text = "+" .. (file.additions or 0), fg = "green" },
      { text = " -" .. (file.deletions or 0), fg = "lightred" },
    }})
  end
  return {
    type = "list",
    id = "files",
    items = items,
    selected = V.diff_file - 1,
    size = { fixed = 34 },
  }
end

local function unified_body(file)
  local r = rows(file)
  local items = {}
  for i, row in ipairs(r) do
    if row.hunk then
      table.insert(items, { spans = { { text = row.text, fg = "cyan", bold = true } } })
    else
      local mark = (i == V.diff_cursor) and ">" or " "
      local num = row.new_lineno or row.old_lineno
      local prefix = (row.kind == "add" and "+") or (row.kind == "del" and "-") or " "
      local bg = line_bg(row.kind)
      table.insert(items, { spans = {
        { text = mark, fg = "yellow", bold = true, bg = bg },
        { text = string.format("%5s ", num and tostring(num) or ""), fg = "darkgray", bg = bg },
        { text = prefix, fg = line_style(row.kind), bg = bg },
        { text = row.text, syntax = file.path, bg = bg },
      }})
      local existing = comment_at(file.path, num, V.diff_sha)
      if existing ~= nil then
        table.insert(items, { spans = { { text = "      > " .. existing, fg = "magenta" } } })
      end
      if i == V.diff_cursor and V.typing ~= nil then
        table.insert(items, { spans = { { text = "      > " .. V.typing .. "_", fg = "magenta" } } })
      end
    end
  end
  return { type = "list", id = "body", items = items, offset = V.diff_scroll }
end

local function split_body(file)
  local r = rows(file)
  local left, right = {}, {}
  for i, row in ipairs(r) do
    if row.hunk then
      table.insert(left, { spans = { { text = row.text, fg = "cyan", bold = true } } })
      table.insert(right, { spans = { { text = "", fg = "cyan" } } })
    else
      local mark = (i == V.diff_cursor) and ">" or " "
      if row.kind == "del" then
        table.insert(left, { spans = { { text = mark .. "-" .. row.text, fg = "lightred" } } })
        table.insert(right, { spans = { { text = "" } } })
      elseif row.kind == "add" then
        table.insert(left, { spans = { { text = "" } } })
        table.insert(right, { spans = { { text = mark .. "+" .. row.text, fg = "green" } } })
      else
        table.insert(left, { spans = { { text = mark .. " " .. row.text, fg = "gray" } } })
        table.insert(right, { spans = { { text = mark .. " " .. row.text, fg = "gray" } } })
      end
    end
  end
  return {
    type = "split",
    direction = "horizontal",
    children = {
      { type = "list", id = "body", items = left, offset = V.diff_scroll },
      { type = "list", items = right, offset = V.diff_scroll },
    },
  }
end

-- Unified diff view for both working tree and commit diffs
local function diff_view(repo)
  local f = cur_file()
  local all_files = diff_files()
  
  if f == nil or #all_files == 0 then
    if V.mode == "commit" and V.diff_sha then
      -- Loading commit diff
      local commit = nil
      for _, l in ipairs(repo.recent or {}) do
        if l.sha == V.diff_sha then commit = l; break end
      end
      local out = {}
      if commit then
        table.insert(out, { type = "text", spans = {
          { text = " Commit ", fg = "cyan", bold = true },
          { text = commit.sha, fg = "yellow", bold = true },
          { text = " - " .. (commit.author or ""), fg = "darkgray" },
        }, size = { fixed = 1 }, wrap = false })
        table.insert(out, { type = "text", spans = {
          { text = " " .. commit.subject, fg = "white", bold = true },
        }, size = { fixed = 1 }, wrap = false })
        table.insert(out, { type = "text", content = "", size = { fixed = 1 } })
        table.insert(out, { type = "text", content = "  Date: " .. (commit.date or ""), fg = "darkgray" })
      end
      table.insert(out, { type = "text", content = "", size = { fixed = 1 } })
      table.insert(out, { type = "text", content = "  No files changed (or diff not loaded yet)", fg = "darkgray" })
      table.insert(out, { type = "spacer", size = { flex = 1 } })
      table.insert(out, { type = "text", spans = {
        { text = "[C-q]", fg = "cyan" }, { text = " back  ", fg = "darkgray" },
        { text = "[g]", fg = "cyan" }, { text = " graph", fg = "darkgray" },
      }, size = { fixed = 1 }, wrap = false })
      return { type = "split", direction = "vertical", children = out }
    end
    return { type = "text", content = "(no changes to review)", fg = "darkgray" }
  end

  -- Build header
  local head
  if V.mode == "commit" then
    head = string.format(" %s  %s  %d/%d", V.diff_sha, f.path, V.diff_file, #all_files)
  else
    head = string.format("%s  %d/%d  %s", f.path, V.diff_file, #all_files, V.split and "split" or "unified")
  end

  local body = V.split and split_body(f) or unified_body(f)

  local children = {
    { type = "text", content = head, fg = "cyan", bold = true, size = { fixed = 1 }, wrap = false },
  }
  if V.tree then
    table.insert(children, { type = "split", direction = "horizontal", children = { file_list(), body } })
  else
    table.insert(children, body)
  end

  -- Bottom bar
  if V.typing ~= nil then
    local prompt = "comment> " .. V.typing .. "_"
    local hint = "  [Enter] save  [C-q] cancel"
    table.insert(children, {
      type = "text", content = prompt .. hint,
      fg = "magenta", size = { min = 1, max = 3 }, wrap = true,
    })
  else
    local spans = {}
    if #V.comments > 0 then
      table.insert(spans, { text = "[C-s]", fg = "cyan" })
      table.insert(spans, { text = string.format(" send %d  ", #V.comments), fg = "yellow" })
    end
    table.insert(spans, { text = "[j/k]", fg = "cyan" })
    table.insert(spans, { text = " nav  ", fg = "darkgray" })
    table.insert(spans, { text = "[n/p]", fg = "cyan" })
    table.insert(spans, { text = " file  ", fg = "darkgray" })
    table.insert(spans, { text = "[c]", fg = "cyan" })
    table.insert(spans, { text = " comment  ", fg = "darkgray" })
    table.insert(spans, { text = "[C-q]", fg = "cyan" })
    table.insert(spans, { text = " back", fg = "darkgray" })
    table.insert(children, { type = "text", spans = spans, size = { fixed = 1 }, wrap = false })
  end

  return { type = "split", direction = "vertical", children = children }
end

function render(state)
  LAST = state
  if state == nil or state.repo == nil then
    return { type = "text", content = "(not a git repository)", fg = "darkgray" }
  end
  if V.mode == "diff" or V.mode == "commit" then
    return diff_view(state.repo)
  elseif V.mode == "graph" then
    return graph_view(state.repo)
  end
  return status_view(state.repo)
end
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff;
    use crate::git::{FileChange, LogEntry};

    fn sample_state() -> GitState {
        GitState {
            branch: Some("main".to_string()),
            ahead: 2,
            behind: 0,
            changes: vec![FileChange {
                status: "M".to_string(),
                path: "a.rs".to_string(),
            }],
            recent: vec![LogEntry {
                sha: "abc1234".to_string(),
                subject: "fix".to_string(),
                author: "dev".to_string(),
                date: "2 hours ago".to_string(),
                refs: vec!["HEAD -> main".to_string()],
                graph: "* ".to_string(),
            }],
            refs: vec![],
            stashes: vec![],
        }
    }

    const SAMPLE_DIFF: &str = "\
diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -10,4 +10,5 @@ fn main() {
 context
-gone
+new one
+new two
";

    fn default_target() -> DiffTarget {
        DiffTarget::WorkingTree
    }

    fn no_commit_diffs() -> CommitDiffs {
        CommitDiffs::new()
    }

    #[test]
    fn not_a_repo_serializes_repo_as_null() {
        let json = state_to_json(None, &[], &default_target(), &no_commit_diffs());
        assert_eq!(json["repo"], serde_json::Value::Null);
        assert_eq!(json["diff"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn state_to_json_round_trips_fields() {
        let json = state_to_json(Some(&sample_state()), &[], &default_target(), &no_commit_diffs());
        assert_eq!(json["repo"]["branch"], "main");
        assert_eq!(json["repo"]["ahead"], 2);
        assert_eq!(json["repo"]["changes"][0]["status"], "M");
        assert_eq!(json["repo"]["recent"][0]["sha"], "abc1234");
    }

    /// The per-line numbers must survive serialization: they are the whole
    /// reason a comment lands on the right line.
    #[test]
    fn diff_json_carries_per_line_numbers() {
        let files = diff::parse(SAMPLE_DIFF);
        let json = state_to_json(Some(&sample_state()), &files, &default_target(), &no_commit_diffs());
        let lines = &json["diff"][0]["hunks"][0]["lines"];

        assert_eq!(lines[0]["kind"], "ctx");
        assert_eq!(lines[0]["new_lineno"], 10);
        // Removed line: absent from the new file, so null rather than a number.
        assert_eq!(lines[1]["kind"], "del");
        assert_eq!(lines[1]["new_lineno"], serde_json::Value::Null);
        assert_eq!(lines[2]["new_lineno"], 11);
        assert_eq!(lines[3]["new_lineno"], 12);
        assert_eq!(json["diff"][0]["additions"], 2);
        assert_eq!(json["diff"][0]["deletions"], 1);
    }

    /// Build a Lua state with the stubs `LUA_SOURCE` expects from the host, so
    /// the chunk can be loaded and exercised without a running TUI.
    fn lua_with_stubs() -> mlua::Lua {
        let lua = mlua::Lua::new();
        let kn9t = lua.create_table().unwrap();
        // Record bindings so tests can invoke them the way the host would.
        let keys = lua.create_table().unwrap();
        let clicks = lua.create_table().unwrap();
        kn9t.set("_keys", keys.clone()).unwrap();
        kn9t.set("_clicks", clicks.clone()).unwrap();
        kn9t.set("_inserted", lua.create_table().unwrap()).unwrap();

        let k = keys.clone();
        kn9t.set(
            "on_key",
            lua.create_function(move |_, (key, f): (String, mlua::Function)| {
                k.set(key, f)?;
                Ok(true)
            })
            .unwrap(),
        )
        .unwrap();

        let c = clicks.clone();
        kn9t.set(
            "on_click",
            lua.create_function(move |_, (id, f): (String, mlua::Function)| {
                c.set(id, f)?;
                Ok(())
            })
            .unwrap(),
        )
        .unwrap();

        kn9t.set(
            "insert_input",
            lua.create_function(|lua, text: String| {
                let kn9t: mlua::Table = lua.globals().get("kn9t")?;
                let inserted: mlua::Table = kn9t.get("_inserted")?;
                inserted.push(text)?;
                Ok(())
            })
            .unwrap(),
        )
        .unwrap();

        kn9t.set(
            "log",
            lua.create_function(|_, _: String| Ok(())).unwrap(),
        )
        .unwrap();

        // Stub for write_file (test only - host provides real one)
        kn9t.set(
            "write_file",
            lua.create_function(|_, (_path, _content): (String, String)| Ok(())).unwrap(),
        )
        .unwrap();

        // Stub for notify - records calls for testing
        kn9t.set("_notified", lua.create_table().unwrap()).unwrap();
        kn9t.set(
            "notify",
            lua.create_function(|lua, data: mlua::Table| {
                let kn9t: mlua::Table = lua.globals().get("kn9t")?;
                let notified: mlua::Table = kn9t.get("_notified")?;
                notified.push(data)?;
                Ok(())
            })
            .unwrap(),
        )
        .unwrap();

        lua.globals().set("kn9t", kn9t).unwrap();
        lua.load(LUA_SOURCE).exec().expect("LUA_SOURCE must parse");
        lua
    }

    fn press(lua: &mlua::Lua, key: &str) {
        let kn9t: mlua::Table = lua.globals().get("kn9t").unwrap();
        let keys: mlua::Table = kn9t.get("_keys").unwrap();
        let f: mlua::Function = keys
            .get(key)
            .unwrap_or_else(|_| panic!("key '{key}' not bound"));
        f.call::<mlua::Value>(key).unwrap();
    }

    fn render_with(lua: &mlua::Lua, json: &serde_json::Value) -> mlua::Table {
        let render: mlua::Function = lua.globals().get("render").unwrap();
        render.call(json_to_lua_for_test(lua, json)).unwrap()
    }

    fn view_state(lua: &mlua::Lua) -> mlua::Table {
        lua.globals().get("V").unwrap()
    }

    #[test]
    fn render_handles_no_repo_and_status_mode() {
        let lua = lua_with_stubs();

        let w = render_with(&lua, &serde_json::Value::Null);
        assert_eq!(w.get::<String>("type").unwrap(), "text");

        let json = state_to_json(Some(&sample_state()), &[], &default_target(), &no_commit_diffs());
        let w = render_with(&lua, &json);
        assert_eq!(w.get::<String>("type").unwrap(), "split");
    }

    /// `d` switches to the review panel, which is a different tree — proving the
    /// plugin's own key binding drives its own layout with no host involvement.
    #[test]
    fn d_toggles_into_diff_mode() {
        let lua = lua_with_stubs();
        let json = state_to_json(Some(&sample_state()), &diff::parse(SAMPLE_DIFF), &default_target(), &no_commit_diffs());

        render_with(&lua, &json); // publish state to LAST
        assert_eq!(view_state(&lua).get::<String>("mode").unwrap(), "status");

        press(&lua, "d");
        assert_eq!(view_state(&lua).get::<String>("mode").unwrap(), "diff");
        let w = render_with(&lua, &json);
        assert_eq!(w.get::<String>("type").unwrap(), "split");
    }

    #[test]
    fn cursor_moves_and_clamps_at_both_ends() {
        let lua = lua_with_stubs();
        let json = state_to_json(Some(&sample_state()), &diff::parse(SAMPLE_DIFF), &default_target(), &no_commit_diffs());
        render_with(&lua, &json);
        press(&lua, "d");

        press(&lua, "j");
        assert_eq!(view_state(&lua).get::<i64>("diff_cursor").unwrap(), 2);
        press(&lua, "k");
        press(&lua, "k");
        assert_eq!(
            view_state(&lua).get::<i64>("diff_cursor").unwrap(),
            1,
            "must not go below the first row"
        );

        // 1 hunk header + 4 lines = 5 rows; walking past the end must clamp.
        for _ in 0..20 {
            press(&lua, "j");
        }
        assert_eq!(view_state(&lua).get::<i64>("diff_cursor").unwrap(), 5);
    }

    #[test]
    fn u_and_b_toggle_split_and_tree() {
        let lua = lua_with_stubs();
        let json = state_to_json(Some(&sample_state()), &diff::parse(SAMPLE_DIFF), &default_target(), &no_commit_diffs());
        render_with(&lua, &json);
        press(&lua, "d");

        assert!(!view_state(&lua).get::<bool>("split").unwrap());
        press(&lua, "u");
        assert!(view_state(&lua).get::<bool>("split").unwrap());
        // Both bodies must still render.
        render_with(&lua, &json);

        assert!(view_state(&lua).get::<bool>("tree").unwrap());
        press(&lua, "b");
        assert!(!view_state(&lua).get::<bool>("tree").unwrap());
        render_with(&lua, &json);
    }

    /// The end-to-end review workflow, and the reason `insert_input` exists.
    #[test]
    fn comment_flow_anchors_to_the_correct_line_and_sends() {
        let lua = lua_with_stubs();
        let json = state_to_json(Some(&sample_state()), &diff::parse(SAMPLE_DIFF), &default_target(), &no_commit_diffs());
        render_with(&lua, &json);
        press(&lua, "d");

        // Row 4 is the first added line, which is new-file line 11.
        press(&lua, "j"); // 2: context (line 10)
        press(&lua, "j"); // 3: removed
        press(&lua, "j"); // 4: added -> 11

        press(&lua, "c");
        for ch in ["o", "d", "d"] {
            press(&lua, ch);
        }
        press(&lua, "Enter");

        let comments: mlua::Table = view_state(&lua).get("comments").unwrap();
        assert_eq!(comments.len().unwrap(), 1);
        let first: mlua::Table = comments.get(1).unwrap();
        assert_eq!(first.get::<String>("text").unwrap(), "odd");
        assert_eq!(
            first.get::<i64>("line").unwrap(),
            11,
            "must use the per-line number, not hunk_start + index"
        );

        press(&lua, "C-s");
        let kn9t: mlua::Table = lua.globals().get("kn9t").unwrap();
        let inserted: mlua::Table = kn9t.get("_inserted").unwrap();
        assert_eq!(inserted.len().unwrap(), 1);
        let text: String = inserted.get(1).unwrap();
        assert!(text.contains("src/main.rs:11"), "got: {text}");
        assert!(text.contains("odd"));
        // Sending clears the queue so the next send is not a duplicate.
        let comments: mlua::Table = view_state(&lua).get("comments").unwrap();
        assert_eq!(comments.len().unwrap(), 0);
    }

    /// When focused, all printable keys must be consumed to prevent typing
    /// in the user input. This is different from unfocused mode.
    #[test]
    fn printable_keys_consumed_when_focused() {
        let lua = lua_with_stubs();
        let kn9t: mlua::Table = lua.globals().get("kn9t").unwrap();
        let keys: mlua::Table = kn9t.get("_keys").unwrap();

        // `o` is a plain printable with no other meaning in this view.
        let f: mlua::Function = keys.get("o").unwrap();
        let consumed: mlua::Value = f.call("o").unwrap();
        assert_eq!(
            consumed,
            mlua::Value::Boolean(true),
            "focused: must consume to prevent typing in user input"
        );
    }

    /// A poll must not move the cursor the user just set.
    #[test]
    fn incoming_state_does_not_reset_view_state() {
        let lua = lua_with_stubs();
        let json = state_to_json(Some(&sample_state()), &diff::parse(SAMPLE_DIFF), &default_target(), &no_commit_diffs());
        render_with(&lua, &json);
        press(&lua, "d");
        press(&lua, "j");
        press(&lua, "u");

        render_with(&lua, &json); // a fresh push arrives

        let v = view_state(&lua);
        assert_eq!(v.get::<String>("mode").unwrap(), "diff");
        assert_eq!(v.get::<i64>("diff_cursor").unwrap(), 2);
        assert!(v.get::<bool>("split").unwrap());
    }

    #[test]
    fn clicking_a_file_row_selects_it() {
        let lua = lua_with_stubs();
        let two = format!(
            "{SAMPLE_DIFF}\
diff --git a/b.rs b/b.rs
--- a/b.rs
+++ b/b.rs
@@ -1 +1 @@
-x
+y
"
        );
        let json = state_to_json(Some(&sample_state()), &diff::parse(&two), &default_target(), &no_commit_diffs());
        render_with(&lua, &json);
        press(&lua, "d");

        let kn9t: mlua::Table = lua.globals().get("kn9t").unwrap();
        let clicks: mlua::Table = kn9t.get("_clicks").unwrap();
        let f: mlua::Function = clicks.get("files").unwrap();
        f.call::<mlua::Value>((0, 1, "left")).unwrap(); // second row

        assert_eq!(view_state(&lua).get::<i64>("diff_file").unwrap(), 2);
    }

    #[test]
    fn diff_mode_with_no_changes_renders_a_message() {
        let lua = lua_with_stubs();
        let json = state_to_json(Some(&sample_state()), &[], &default_target(), &no_commit_diffs());
        render_with(&lua, &json);
        press(&lua, "d");
        let w = render_with(&lua, &json);
        assert_eq!(w.get::<String>("type").unwrap(), "text");
    }

    /// Minimal JSON->Lua conversion for the tests above. Deliberately NOT the
    /// production path (that lives host-side in kn9t-tui's plugin_ui.rs) — but
    /// it must agree on the one thing that matters here: JSON null becomes nil,
    /// which is how `new_lineno` absence is represented.
    fn json_to_lua_for_test(lua: &mlua::Lua, v: &serde_json::Value) -> mlua::Value {
        match v {
            serde_json::Value::Null => mlua::Value::Nil,
            serde_json::Value::Bool(b) => mlua::Value::Boolean(*b),
            serde_json::Value::Number(n) => match n.as_i64() {
                Some(i) => mlua::Value::Integer(i),
                None => mlua::Value::Number(n.as_f64().unwrap_or(0.0)),
            },
            serde_json::Value::String(s) => mlua::Value::String(lua.create_string(s).unwrap()),
            serde_json::Value::Array(arr) => {
                let t = lua.create_table().unwrap();
                for (i, item) in arr.iter().enumerate() {
                    t.set(i + 1, json_to_lua_for_test(lua, item)).unwrap();
                }
                mlua::Value::Table(t)
            }
            serde_json::Value::Object(map) => {
                let t = lua.create_table().unwrap();
                for (k, val) in map {
                    t.set(k.as_str(), json_to_lua_for_test(lua, val)).unwrap();
                }
                mlua::Value::Table(t)
            }
        }
    }

    fn get_notified(lua: &mlua::Lua) -> Vec<(String, Option<String>)> {
        let kn9t: mlua::Table = lua.globals().get("kn9t").unwrap();
        let notified: mlua::Table = kn9t.get("_notified").unwrap();
        let mut out = Vec::new();
        for i in 1..=notified.len().unwrap_or(0) {
            if let Ok(entry) = notified.get::<mlua::Table>(i) {
                let event: String = entry.get("event").unwrap_or_default();
                let sha: Option<String> = entry.get("sha").ok();
                out.push((event, sha));
            }
        }
        out
    }

    #[test]
    fn enter_in_graph_mode_calls_notify_with_commit_sha() {
        let lua = lua_with_stubs();
        let json = state_to_json(Some(&sample_state()), &[], &default_target(), &no_commit_diffs());
        render_with(&lua, &json);

        // Switch to graph mode
        press(&lua, "g");
        assert_eq!(view_state(&lua).get::<String>("mode").unwrap(), "graph");

        // Press Enter to view the commit
        press(&lua, "Enter");

        // Should have switched to commit mode
        assert_eq!(view_state(&lua).get::<String>("mode").unwrap(), "commit");

        // Should have called kn9t.notify with request_commit_diff
        let notified = get_notified(&lua);
        assert_eq!(notified.len(), 1, "expected one notify call, got {:?}", notified);
        assert_eq!(notified[0].0, "request_commit_diff");
        assert_eq!(notified[0].1, Some("abc1234".to_string()));
    }

    #[test]
    fn backspace_in_commit_mode_calls_notify_to_clear() {
        let lua = lua_with_stubs();
        let json = state_to_json(Some(&sample_state()), &[], &default_target(), &no_commit_diffs());
        render_with(&lua, &json);

        // Go to graph mode then commit mode
        press(&lua, "g");
        press(&lua, "Enter");
        assert_eq!(view_state(&lua).get::<String>("mode").unwrap(), "commit");

        // Clear notified list to only catch the backspace
        let kn9t: mlua::Table = lua.globals().get("kn9t").unwrap();
        kn9t.set("_notified", lua.create_table().unwrap()).unwrap();

        // Press Backspace to go back
        press(&lua, "Backspace");
        assert_eq!(view_state(&lua).get::<String>("mode").unwrap(), "graph");

        // Should have called kn9t.notify with clear_commit_diff
        let notified = get_notified(&lua);
        assert_eq!(notified.len(), 1, "expected one notify call, got {:?}", notified);
        assert_eq!(notified[0].0, "clear_commit_diff");
    }

    #[test]
    fn click_on_graph_row_calls_notify() {
        let lua = lua_with_stubs();
        let json = state_to_json(Some(&sample_state()), &[], &default_target(), &no_commit_diffs());
        render_with(&lua, &json);

        // Switch to graph mode
        press(&lua, "g");

        // Clear notified
        let kn9t: mlua::Table = lua.globals().get("kn9t").unwrap();
        kn9t.set("_notified", lua.create_table().unwrap()).unwrap();

        // Click on the first commit row
        let clicks: mlua::Table = kn9t.get("_clicks").unwrap();
        let f: mlua::Function = clicks.get("graph").unwrap();
        f.call::<mlua::Value>((0, 0, "left")).unwrap();

        // Should have called kn9t.notify
        let notified = get_notified(&lua);
        assert_eq!(notified.len(), 1, "expected one notify call from click, got {:?}", notified);
        assert_eq!(notified[0].0, "request_commit_diff");
    }
}
