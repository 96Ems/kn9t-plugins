// kn9t-agents-md: AGENTS.md discovery and injection plugin for kn9t.
//
// Persistence model:
//   Each session's injected set is stored in the host KV store (plugin_kv):
//     plugin = "kn9t-agents-md"  (set by host, namespaced)
//     scope  = <session_id>
//     key    = "injected"
//     value  = JSON array of absolute paths already injected
//
//   This survives kn9t-server restarts.  On session delete the host removes
//   the scope automatically.  On compaction the plugin also reacts to the
//   "compacted" bus event to call kv_del_scope so re-injection happens.
//
// Wire protocol: kn9t plugin v2 (newline-delimited JSON over stdin/stdout)
package main

import (
	"bufio"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

// ── Wire protocol types ──────────────────────────────────────────────────────

type HostMsg struct {
	T       string          `json:"t"`
	Proto   int             `json:"proto,omitempty"`
	Kn9t    string          `json:"kn9t,omitempty"`
	ID      uint64          `json:"id,omitempty"`
	Hook    string          `json:"hook,omitempty"`
	Payload json.RawMessage `json:"payload,omitempty"`
	// kv_result fields
	Value *json.RawMessage `json:"value,omitempty"`
	Ok    bool             `json:"ok,omitempty"`
	Error *string          `json:"error,omitempty"`
}

type PluginHello struct {
	T            string   `json:"t"`
	Name         string   `json:"name"`
	Capabilities []string `json:"capabilities"`
	Hooks        []string `json:"hooks"`
	Tools        []any    `json:"tools"`
	Events       []string `json:"events"`
}

// PluginEvent sends a fire-and-forget notification to the host's EventBus.
// Format: plugin name + message to display. TUI shows "ℹ {plugin}: {message}".
// MUST include session_id for routing (events without session_id are dropped).
type PluginEvent struct {
	T         string `json:"t"`          // Always "event"
	Plugin    string `json:"plugin"`     // Plugin name for display
	Message   string `json:"message"`    // Human-readable message to display
	SessionID string `json:"session_id"` // Required for routing
}

// KV request messages (plugin → host). Host replies with kv_result.

type KvGetMsg struct {
	T     string `json:"t"`
	ID    uint64 `json:"id"`
	Scope string `json:"scope"`
	Key   string `json:"key"`
}

type KvSetMsg struct {
	T     string          `json:"t"`
	ID    uint64          `json:"id"`
	Scope string          `json:"scope"`
	Key   string          `json:"key"`
	Value json.RawMessage `json:"value"`
}

type KvDelScopeMsg struct {
	T     string `json:"t"`
	ID    uint64 `json:"id"`
	Scope string `json:"scope"`
}

// kvReply is the decoded kv_result payload routed back to the waiting goroutine.
type kvReply struct {
	value *json.RawMessage
	ok    bool
	err   *string
}

// ── Hook payloads ────────────────────────────────────────────────────────────

type AfterToolCallPayload struct {
	SessionID string          `json:"session_id"`
	Tool      string          `json:"tool"`
	Args      json.RawMessage `json:"args"`
	Cwd       string          `json:"cwd"`
	Result    json.RawMessage `json:"result"`
}

type GetSteeringPayload struct {
	SessionID string `json:"session_id"`
	Cwd       string `json:"cwd"`
}

type ReadArgs struct {
	Path     string `json:"path"`
	FilePath string `json:"filePath"`
}

type GlobArgs struct {
	Pattern string `json:"pattern"`
	Path    string `json:"path"`
}

type GrepArgs struct {
	Pattern string `json:"pattern"`
	Path    string `json:"path"`
	Include string `json:"include"`
}

// ── Steering message format ──────────────────────────────────────────────────

type Message struct {
	Role    string    `json:"role"`
	Content []Content `json:"content"`
	Silent  bool      `json:"silent,omitempty"` // If true, persisted but not displayed in TUI
}

type Content struct {
	Type string `json:"type"`
	Text string `json:"text"`
}

// ── Plugin state ─────────────────────────────────────────────────────────────

type PendingAgentsMd struct {
	Path    string
	Content string
	Source  string // "global", "project", "directory"
}

// Plugin holds all runtime state.  Session injection tracking is stored in the
// host KV store (survives server restarts); only in-flight pending queues live here.
//
// There is deliberately no `workspaceRoot` field. A plugin is a long-lived subprocess
// spawned by the server, so os.Getwd() inside it is wherever the *server* was started —
// unrelated to any session, and shared by every session at once. The host sends the
// session's own directory as `cwd` on every hook payload; that value is threaded through
// the call instead of being cached here, so two sessions rooted in different repos cannot
// be confused for one another.
type Plugin struct {
	globalConfig string

	// stdout writer — all output serialised through writerMu.
	writer   *bufio.Writer
	writerMu sync.Mutex

	// Monotonically increasing ID for KV requests.
	nextID atomic.Uint64

	// KV pending map: request ID → reply channel.
	// The reader goroutine delivers replies here; blocked callers receive them.
	kvPendingMu sync.Mutex
	kvPending   map[uint64]chan kvReply

	// hookCh carries hook/event/shutdown messages from the reader goroutine
	// to the main dispatch loop.
	hookCh chan HostMsg

	// Per-session pending queues (items discovered this turn, not yet injected).
	// These are ephemeral: the KV store is the durable truth.
	pendingMu sync.Mutex
	pending   map[string][]PendingAgentsMd
}

func NewPlugin() *Plugin {
	p := &Plugin{
		globalConfig:  getGlobalConfigPath(),
		writer:        bufio.NewWriter(os.Stdout),
		kvPending:     make(map[uint64]chan kvReply),
		hookCh:        make(chan HostMsg, 64),
		pending:       make(map[string][]PendingAgentsMd),
	}
	p.nextID.Store(1_000_000)
	return p
}

func getGlobalConfigPath() string {
	if home := os.Getenv("HOME"); home != "" {
		return filepath.Join(home, ".kn9t")
	}
	if home := os.Getenv("USERPROFILE"); home != "" {
		return filepath.Join(home, ".kn9t")
	}
	return ".kn9t"
}

// ── Main loop ────────────────────────────────────────────────────────────────

func main() {
	plugin := NewPlugin()
	plugin.Run()
}

func (p *Plugin) Run() {
	scanner := bufio.NewScanner(os.Stdin)

	// Read host hello synchronously before starting goroutine.
	if !scanner.Scan() {
		fmt.Fprintln(os.Stderr, "Failed to read host hello")
		return
	}
	var hello HostMsg
	if err := json.Unmarshal(scanner.Bytes(), &hello); err != nil || hello.T != "hello" {
		fmt.Fprintln(os.Stderr, "Expected hello, got:", string(scanner.Bytes()))
		return
	}
	fmt.Fprintf(os.Stderr, "Connected to kn9t %s (proto %d)\n", hello.Kn9t, hello.Proto)

	// Send plugin hello.
	p.sendHello()

	// Reader goroutine: parse every line from stdin and route:
	//   kv_result  → kvPending map (unblocks waiting KV call)
	//   everything else → hookCh (main loop below)
	go func() {
		for scanner.Scan() {
			var msg HostMsg
			if err := json.Unmarshal(scanner.Bytes(), &msg); err != nil {
				fmt.Fprintln(os.Stderr, "Invalid message:", err)
				continue
			}
			if msg.T == "kv_result" {
				p.kvPendingMu.Lock()
				ch, ok := p.kvPending[msg.ID]
				p.kvPendingMu.Unlock()
				if ok {
					ch <- kvReply{value: msg.Value, ok: msg.Ok, err: msg.Error}
				}
				continue
			}
			p.hookCh <- msg
		}
		close(p.hookCh)
	}()

	// Main dispatch loop — receives non-KV messages.
	for msg := range p.hookCh {
		switch msg.T {
		case "hook":
			p.handleHook(msg.ID, msg.Hook, msg.Payload)
		case "event":
			p.handleEvent(msg.Payload)
		case "shutdown":
			fmt.Fprintln(os.Stderr, "Shutdown requested")
			return
		}
	}
}

// handleEvent processes bus events forwarded by the host.
func (p *Plugin) handleEvent(payload json.RawMessage) {
	var event struct {
		Kind      string `json:"kind"`
		SessionID string `json:"session_id"`
	}
	if err := json.Unmarshal(payload, &event); err != nil {
		return
	}
	switch event.Kind {
	case "compacted":
		// Clear the KV scope so AGENTS.md is re-injected after compaction.
		if event.SessionID != "" {
			if err := p.kvDelScope(event.SessionID); err != nil {
				fmt.Fprintf(os.Stderr, "kv_del_scope(%s): %v\n", event.SessionID, err)
			}
			// Also drop any in-process pending queue for this session.
			p.pendingMu.Lock()
			delete(p.pending, event.SessionID)
			p.pendingMu.Unlock()
		}
		fmt.Fprintf(os.Stderr, "Compacted session %s — KV scope cleared\n", event.SessionID)
	}
}

func (p *Plugin) sendHello() {
	hello := PluginHello{
		T:            "hello",
		Name:         "kn9t-agents-md",
		Capabilities: []string{},
		Hooks:        []string{"after_tool_call", "get_steering"},
		Tools:        []any{},
		Events:       []string{"compacted"},
	}
	p.send(hello)
}

func (p *Plugin) send(v any) {
	data, _ := json.Marshal(v)
	p.writerMu.Lock()
	p.writer.Write(data)
	p.writer.WriteByte('\n')
	p.writer.Flush()
	p.writerMu.Unlock()
}

// sendResult sends a hook response with body fields flattened at the top level.
func (p *Plugin) sendResult(id uint64, body map[string]any) {
	result := map[string]any{"t": "result", "id": id}
	for k, v := range body {
		result[k] = v
	}
	p.send(result)
}

// ── Hook handlers ────────────────────────────────────────────────────────────

func (p *Plugin) handleHook(id uint64, hook string, payload json.RawMessage) {
	switch hook {
	case "after_tool_call":
		p.handleAfterToolCall(id, payload)
	case "get_steering":
		p.handleGetSteering(id, payload)
	default:
		p.sendResult(id, map[string]any{})
	}
}

func (p *Plugin) handleAfterToolCall(id uint64, payload json.RawMessage) {
	var data AfterToolCallPayload
	if err := json.Unmarshal(payload, &data); err != nil {
		p.sendResult(id, map[string]any{"action": "keep"})
		return
	}
	paths := p.extractPaths(data.Tool, data.Args)
	for _, path := range paths {
		// The session's cwd is both how a relative tool path is resolved and where the
		// walk-up stops. It used to stop at the startup cwd, which for any session not
		// rooted there either ended the walk immediately or let it climb out of the
		// project — see discoverFromPath.
		p.discoverFromPath(data.SessionID, path, data.Cwd)
	}
	p.sendResult(id, map[string]any{"action": "keep"})
}

func (p *Plugin) handleGetSteering(id uint64, payload json.RawMessage) {
	var data GetSteeringPayload
	json.Unmarshal(payload, &data)
	sid := data.SessionID
	if sid == "" {
		sid = "_default"
	}

	// Use cwd from payload. Without one there is no project root to resolve against,
	// and guessing os.Getwd() would attach this session to whatever directory the
	// *plugin process* started in — a different repo, most likely, and the same wrong
	// one for every session at once. Global AGENTS.md still applies.
	workspaceRoot := data.Cwd

	// Ensure global + project AGENTS.md are queued for this session.
	p.ensureInitialWithCwd(sid, workspaceRoot)

	// Drain the pending queue and build steering messages.
	messages := p.buildSteeringMessages(sid)
	p.sendResult(id, map[string]any{"messages": messages})
}

// ── Path extraction ──────────────────────────────────────────────────────────

func (p *Plugin) extractPaths(tool string, argsRaw json.RawMessage) []string {
	switch tool {
	case "read":
		var args ReadArgs
		if err := json.Unmarshal(argsRaw, &args); err != nil {
			return nil
		}
		if args.Path != "" {
			return []string{args.Path}
		}
		if args.FilePath != "" {
			return []string{args.FilePath}
		}
	case "glob":
		var args GlobArgs
		if err := json.Unmarshal(argsRaw, &args); err != nil {
			return nil
		}
		if args.Path != "" {
			return []string{args.Path}
		}
	case "grep":
		var args GrepArgs
		if err := json.Unmarshal(argsRaw, &args); err != nil {
			return nil
		}
		if args.Path != "" {
			return []string{args.Path}
		}
	}
	return nil
}

// ── AGENTS.md discovery ──────────────────────────────────────────────────────

// ensureInitialWithCwd queues the global and project AGENTS.md for a session,
// using the provided cwd as the project root. An empty workspaceRoot means the host
// reported no cwd: the global file is still queued, the project one is skipped rather
// than resolved against a guess.
func (p *Plugin) ensureInitialWithCwd(sessionID, workspaceRoot string) {
	p.queueIfNew(sessionID, filepath.Join(p.globalConfig, "AGENTS.md"), "global")
	if workspaceRoot == "" {
		return
	}
	p.queueIfNew(sessionID, filepath.Join(workspaceRoot, "AGENTS.md"), "project")
}

// discoverFromPath queues AGENTS.md for the touched file's directory and each parent up
// to `workspaceRoot`, which is the session's cwd.
//
// `filePath` may be relative (the model writes `src/main.rs`), so it is resolved against
// workspaceRoot first — otherwise os.Stat and the walk both operate on a path relative to
// the plugin process's directory. With no workspaceRoot there is no root to stop at and a
// relative path cannot be placed at all, so nothing is queued: an unbounded walk up from a
// guessed directory would inject AGENTS.md files from unrelated projects.
func (p *Plugin) discoverFromPath(sessionID, filePath, workspaceRoot string) {
	if workspaceRoot == "" {
		return
	}
	if !filepath.IsAbs(filePath) {
		filePath = filepath.Join(workspaceRoot, filePath)
	}
	dir := filePath
	if info, err := os.Stat(filePath); err == nil && !info.IsDir() {
		dir = filepath.Dir(filePath)
	}
	current := dir
	for {
		p.queueIfNew(sessionID, filepath.Join(current, "AGENTS.md"), "directory")
		if current == workspaceRoot {
			break
		}
		parent := filepath.Dir(current)
		if !strings.HasPrefix(parent, workspaceRoot) && parent != workspaceRoot {
			break
		}
		if parent == current {
			break
		}
		current = parent
	}
}

// queueIfNew reads the KV store to check whether absPath has already been
// injected into sessionID.  If not, it reads the file, marks it injected in
// the KV store, and appends it to the in-process pending queue.
func (p *Plugin) queueIfNew(sessionID, path, source string) {
	absPath, err := filepath.Abs(path)
	if err != nil {
		return
	}

	// Load the injected set from KV.
	injected := p.kvGetInjected(sessionID)
	if injected[absPath] {
		return // already done, don't re-inject
	}

	// Try to read the file — it may not exist.
	content, err := os.ReadFile(absPath)
	if err != nil {
		return
	}

	// Persist the updated injected set before queuing (idempotent on restart).
	injected[absPath] = true
	if err := p.kvSetInjected(sessionID, injected); err != nil {
		fmt.Fprintf(os.Stderr, "kv_set injected(%s): %v\n", absPath, err)
		// Proceed anyway — worst case is a duplicate injection if the process
		// dies between here and the next get_steering.
	}

	p.pendingMu.Lock()
	p.pending[sessionID] = append(p.pending[sessionID], PendingAgentsMd{
		Path:    absPath,
		Content: string(content),
		Source:  source,
	})
	p.pendingMu.Unlock()

	fmt.Fprintf(os.Stderr, "Discovered AGENTS.md: %s (%s)\n", absPath, source)
}

// ── Steering message building ────────────────────────────────────────────────

func (p *Plugin) buildSteeringMessages(sessionID string) []Message {
	p.pendingMu.Lock()
	items := p.pending[sessionID]
	delete(p.pending, sessionID)
	p.pendingMu.Unlock()

	messages := make([]Message, 0, len(items))
	for _, item := range items {
		lines := strings.Count(item.Content, "\n") + 1
		p.sendNotification(sessionID, fmt.Sprintf("Loaded %s (%s, %d lines)", item.Path, item.Source, lines))
		text := fmt.Sprintf(
			"<system-reminder source=\"AGENTS.md: %s (%s, %d lines)\">\n%s\n</system-reminder>",
			item.Path, item.Source, lines, item.Content,
		)
		messages = append(messages, Message{
			Role:   "user",
			Silent: true,
			Content: []Content{{Type: "text", Text: text}},
		})
	}
	return messages
}

func (p *Plugin) sendNotification(sessionID, message string) {
	p.send(PluginEvent{T: "event", Plugin: "kn9t-agents-md", Message: message, SessionID: sessionID})
}

// ── KV helpers ───────────────────────────────────────────────────────────────

const kvKey = "injected"

// kvGetInjected loads the set of already-injected paths for a session from KV.
// Returns an empty map on miss or error.
func (p *Plugin) kvGetInjected(sessionID string) map[string]bool {
	id := p.nextID.Add(1)
	ch := make(chan kvReply, 1)
	p.kvPendingMu.Lock()
	p.kvPending[id] = ch
	p.kvPendingMu.Unlock()

	msg := KvGetMsg{T: "kv_get", ID: id, Scope: sessionID, Key: kvKey}
	data, _ := json.Marshal(msg)
	p.writerMu.Lock()
	p.writer.Write(data)
	p.writer.WriteByte('\n')
	p.writer.Flush()
	p.writerMu.Unlock()

	select {
	case r := <-ch:
		p.kvPendingMu.Lock()
		delete(p.kvPending, id)
		p.kvPendingMu.Unlock()
		if !r.ok || r.value == nil {
			return make(map[string]bool)
		}
		var paths []string
		if err := json.Unmarshal(*r.value, &paths); err != nil {
			return make(map[string]bool)
		}
		out := make(map[string]bool, len(paths))
		for _, p := range paths {
			out[p] = true
		}
		return out
	case <-time.After(5 * time.Second):
		p.kvPendingMu.Lock()
		delete(p.kvPending, id)
		p.kvPendingMu.Unlock()
		fmt.Fprintln(os.Stderr, "kv_get timeout for session", sessionID)
		return make(map[string]bool)
	}
}

// kvSetInjected persists the injected set for a session.
func (p *Plugin) kvSetInjected(sessionID string, injected map[string]bool) error {
	paths := make([]string, 0, len(injected))
	for k := range injected {
		paths = append(paths, k)
	}
	valueJSON, _ := json.Marshal(paths)

	id := p.nextID.Add(1)
	ch := make(chan kvReply, 1)
	p.kvPendingMu.Lock()
	p.kvPending[id] = ch
	p.kvPendingMu.Unlock()

	msg := KvSetMsg{T: "kv_set", ID: id, Scope: sessionID, Key: kvKey, Value: json.RawMessage(valueJSON)}
	data, _ := json.Marshal(msg)
	p.writerMu.Lock()
	p.writer.Write(data)
	p.writer.WriteByte('\n')
	p.writer.Flush()
	p.writerMu.Unlock()

	select {
	case r := <-ch:
		p.kvPendingMu.Lock()
		delete(p.kvPending, id)
		p.kvPendingMu.Unlock()
		if !r.ok {
			if r.err != nil {
				return fmt.Errorf("%s", *r.err)
			}
			return fmt.Errorf("kv_set failed")
		}
		return nil
	case <-time.After(5 * time.Second):
		p.kvPendingMu.Lock()
		delete(p.kvPending, id)
		p.kvPendingMu.Unlock()
		return fmt.Errorf("kv_set timeout")
	}
}

// kvDelScope removes all KV entries for a session scope.
func (p *Plugin) kvDelScope(sessionID string) error {
	id := p.nextID.Add(1)
	ch := make(chan kvReply, 1)
	p.kvPendingMu.Lock()
	p.kvPending[id] = ch
	p.kvPendingMu.Unlock()

	msg := KvDelScopeMsg{T: "kv_del_scope", ID: id, Scope: sessionID}
	data, _ := json.Marshal(msg)
	p.writerMu.Lock()
	p.writer.Write(data)
	p.writer.WriteByte('\n')
	p.writer.Flush()
	p.writerMu.Unlock()

	select {
	case r := <-ch:
		p.kvPendingMu.Lock()
		delete(p.kvPending, id)
		p.kvPendingMu.Unlock()
		if !r.ok {
			if r.err != nil {
				return fmt.Errorf("%s", *r.err)
			}
			return fmt.Errorf("kv_del_scope failed")
		}
		return nil
	case <-time.After(5 * time.Second):
		p.kvPendingMu.Lock()
		delete(p.kvPending, id)
		p.kvPendingMu.Unlock()
		return fmt.Errorf("kv_del_scope timeout")
	}
}
