#!/usr/bin/env node
/**
 * PreToolUse hook (WARN-ONLY): reminds when an Edit/Write/NotebookEdit targets
 * the root checkout instead of a worktree lane.
 *
 * AGENTS.md rule 8: prefer a worktree lane for all file-changing work; the root
 * checkout is a pristine, current mirror of `main`. The operator selected
 * WARN-ONLY enforcement (not a hard block), so this hook NEVER blocks a call —
 * it emits a non-blocking `systemMessage` reminder and always exits 0 (allow).
 *
 * Compliant (no reminder): paths under `.claude/worktrees/**` (the harness
 * EnterWorktree default) and `.worktrees/**` (manual lanes), and any path
 * outside the repository entirely (e.g. ~/.claude, /tmp scratch).
 *
 * Any unexpected failure simply allows the call — this hook is an advisory, not
 * a gate (exit 2 would block; we never do that here).
 */
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import path from "node:path";

function allow() {
  process.exit(0);
}

let input;
try {
  input = JSON.parse(readFileSync(0, "utf8"));
} catch {
  allow();
}

const toolInput = input.tool_input ?? {};
const target = toolInput.file_path ?? toolInput.notebook_path;
if (typeof target !== "string" || target === "") {
  allow();
}

const cwd =
  typeof input.cwd === "string" && input.cwd !== "" ? input.cwd : process.cwd();

let mainRoot;
try {
  // The common git dir is <main-root>/.git for every linked worktree.
  const commonDir = execFileSync(
    "git",
    ["rev-parse", "--path-format=absolute", "--git-common-dir"],
    { cwd, encoding: "utf8" },
  ).trim();
  mainRoot = path.dirname(commonDir);
} catch {
  allow(); // not inside a git repository — nothing to remind about
}

const resolved = path.resolve(cwd, target);
const rel = path.relative(mainRoot, resolved);
const insideRoot =
  rel === "" || (!rel.startsWith("..") && !path.isAbsolute(rel));
const insideLane =
  rel.startsWith(`.worktrees${path.sep}`) ||
  rel.startsWith(`.claude${path.sep}worktrees${path.sep}`);

if (insideRoot && !insideLane) {
  const msg =
    `Reminder (AGENTS.md rule 8): ${resolved} is in the root checkout. ` +
    `Prefer a worktree lane — see the worktree-lane skill ` +
    `(git worktree add --detach .claude/worktrees/<lane> HEAD), then edit there. ` +
    `Warn-only: proceeding with this edit.`;
  // Non-blocking warning surfaced to the operator; the tool call still runs.
  process.stdout.write(JSON.stringify({ systemMessage: msg }));
}
allow();
