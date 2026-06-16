# Runbook — local persistent memory MCP (`memory` / mcp-server-qdrant)

## What it is and why

A repository-scoped, fully-local vector-RAG memory for coding-agent sessions:
decisions, operator feedback, gotchas, milestone state that future sessions must
recall. It is an embedded Qdrant DB + a local ONNX embedding model (FastEmbed) —
free, no API keys, no paid tier, nothing leaves the machine after the first model
fetch. It is the **repository-scoped, committed-config, team-shared** memory;
conventions live in the [`memory` skill](../../.claude/skills/memory/SKILL.md).
Decision: [ADR-G006](../decisions/ADR-G006.md).

## Resource identity

- **MCP server name:** `memory` (stdio), configured in [`/.mcp.json`](../../.mcp.json).
- **Server package:** `mcp-server-qdrant@0.8.1`, run via `uvx` (no global install).
- **Runner:** `uv` / `uvx` (Astral) at `~/.local/bin` — installed **0.11.21**, verified 2026-06-16.
- **Store path:** `${CLAUDE_PROJECT_DIR}/.memory/qdrant` (collection `memory`).
- **Embedding-model cache:** `${CLAUDE_PROJECT_DIR}/.memory/fastembed_cache`.
- **Tools exposed:** `qdrant-find`, `qdrant-store` (descriptions overridden via
  `TOOL_FIND_DESCRIPTION` / `TOOL_STORE_DESCRIPTION` so the agent recalls/stores
  proactively).
- `.memory/` is gitignored — **never commit it**.

## Create / install (already done)

```bash
# 1. Install the uvx runner (idempotent; ~/.local/bin must be on PATH)
command -v uvx || curl -LsSf https://astral.sh/uv/install.sh | sh

# 2. Warm the package cache (downloads ~68 deps incl. onnxruntime/fastembed)
uvx mcp-server-qdrant@0.8.1 --help

# 3. .mcp.json + .claude/settings.json (enableAllProjectMcpServers: true) are
#    committed. The server loads on the next Claude Code session start.
```

The HuggingFace embedding model (~100 MB) downloads on the **first** `qdrant-store`
/`qdrant-find` call, then the server is fully offline. On a locked-down/offline box,
warm it once with network access first.

## Verify

1. Restart the Claude Code session (MCP servers load at startup).
2. `qdrant-store` a test entry, then `qdrant-find` for it — the find returns it:
   - store: `"Memory MCP verified on 2026-06-16."`, metadata `{"type":"reference","topic":"memory-mcp"}`
   - find: `"is the memory mcp working?"` → returns the entry.
3. `uvx --version` and `command -v uvx` resolve (PATH includes `~/.local/bin`).

## Rotate / recreate / restore / roll back

- **Recreate the store:** stop all sessions, `rm -rf .memory/qdrant`, restart — a
  fresh empty collection is created. (This discards all stored memories.)
- **Re-fetch the model:** `rm -rf .memory/fastembed_cache`; the next call
  re-downloads it.
- **Bump the server:** edit the `mcp-server-qdrant@<version>` pin in `.mcp.json`,
  `uvx mcp-server-qdrant@<version> --help` to warm, restart. Roll back by
  restoring the previous pin.
- **No delete tool** for individual memories: store a correcting entry stating both
  the old claim and the correction.

## Gotchas

- **Single-process lock:** only one session per repo clone can hold the store. A
  second concurrent session's `memory` server fails to connect — that is the lock,
  not corruption. The lock belongs to the **root** session (`.mcp.json` paths point
  at the root checkout's `.memory/`); worktree sessions don't get it.
- `~/.local/bin` must be on the PATH the Claude Code process sees, or the `uvx`
  command in `.mcp.json` fails to launch the server.
- `${CLAUDE_PROJECT_DIR}` is expanded by Claude Code to the project root; if a
  future host doesn't expand it, substitute the absolute repo path in `.mcp.json`.
