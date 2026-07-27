#!/usr/bin/env node
/**
 * PreToolUse hook — keep file reads and searches from dumping into context.
 *
 * Why: measured at 3,179 Bash calls against 165 Read calls (19:1). Bash is
 * 74.6% of all tool-result bytes and ~29% of request payload. Every Bash call
 * is a full turn (~12s) whose output then rides in the cached prefix for every
 * remaining turn of the session. cache_read is 59.5% of true spend.
 *
 * ADAPTED FROM THE UPSTREAM SWEEP-2 HOOK. Upstream redirected searches to the
 * `Grep` and `Glob` tools. **Those two tools do not exist in this harness** —
 * verified 2026-07-27 in an interactive session, and reported independently for
 * subagents and for headless `claude -p`, which is exactly what tick.sh spawns.
 * A block whose only remedy is a tool that cannot be called is worse than the
 * burn it prevents, so the search rules bound output instead of redirecting:
 *
 *   cat / head / tail / sed -n M,Np  -> Read          (Read DOES exist)
 *   rg / grep unbounded              -> add -l, -c, -m N, or `| head -n 50`
 *   find / ls -R unbounded           -> add `| head -n 50`
 *
 * A segment whose stdout is piped into something else never reaches context at
 * all, so only the last segment of a pipeline is checked for the search family.
 *
 * Fails OPEN: any parse error, unknown shape, or unexpected exception allows
 * the command. A hook that blocks legitimate work is worse than one that misses.
 *
 * Escape hatch: NATIVE_TOOL_HOOK=off, or prefix the command with `# raw:` when
 * you genuinely need the unbounded shell form.
 *
 * Install in .claude/settings.json:
 *   "hooks": { "PreToolUse": [ { "matcher": "Bash",
 *     "hooks": [ { "type": "command", "command": "node \"$CLAUDE_PROJECT_DIR/.claude/hooks/prefer-native-tools.mjs\"" } ] } ] }
 */

import { readFileSync } from 'node:fs';

const ALLOW = 0;
const BLOCK = 2; // exit 2 => blocked, stderr is shown to the agent

function readStdin() {
  try {
    return readFileSync(0, 'utf8');
  } catch {
    return '';
  }
}

/** Split a command line into pipeline segments, ignoring separators inside quotes. */
function segments(cmd) {
  const out = [];
  let buf = '';
  let quote = null;
  for (let i = 0; i < cmd.length; i++) {
    const c = cmd[i];
    if (quote) {
      if (c === quote && cmd[i - 1] !== '\\') quote = null;
      buf += c;
      continue;
    }
    if (c === '"' || c === "'") { quote = c; buf += c; continue; }
    if (c === '|' || c === ';' || c === '&') {
      if (buf.trim()) out.push(buf.trim());
      buf = '';
      // consume doubled operators
      while (i + 1 < cmd.length && (cmd[i + 1] === '|' || cmd[i + 1] === '&')) i++;
      continue;
    }
    buf += c;
  }
  if (buf.trim()) out.push(buf.trim());
  return out;
}

/** True when the segment reads piped stdin rather than naming files. */
function isDownstream(cmd, seg) {
  const idx = cmd.indexOf(seg);
  if (idx <= 0) return false;
  const before = cmd.slice(0, idx);
  // last unquoted separator before this segment was a pipe
  const m = before.match(/([|;&])[^|;&]*$/);
  return !!m && m[1] === '|';
}

/**
 * True when this segment's stdout is piped onward. Its bytes are consumed by
 * the next stage and never enter the transcript, so output size is not our
 * problem — only the tail of a pipeline reaches context.
 */
function pipesOut(cmd, seg) {
  const idx = cmd.indexOf(seg);
  if (idx < 0) return false;
  const after = cmd.slice(idx + seg.length);
  return /^\s*\|(?!\|)/.test(after);
}

function tokenise(seg) {
  return seg.match(/(?:[^\s"']+|"[^"]*"|'[^']*')+/g) || [];
}

// Flags that swallow the following token as their value.
const VALUE_FLAGS = new Set([
  '-m', '--max-count', '-A', '-B', '-C', '--after-context', '--before-context',
  '--context', '-e', '--regexp', '-f', '--file', '-g', '--glob', '-t', '--type',
  '--include', '--exclude', '--exclude-dir', '-d', '--color', '--colour',
  '--iglob', '-M', '--max-columns', '--sort', '-name', '-iname', '-path',
  '-type', '-maxdepth', '-mindepth', '-newer', '-size', '-perm', '-user',
]);

/** Bounded search: -l/-L/-c/-q (incl. combined shorts) or an explicit -m/--max-count. */
function isBoundedSearch(args) {
  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    if (a === '-m' || a === '--max-count' || a.startsWith('-m') && /^-m\d+$/.test(a)) return true;
    if (/^--max-count(=|$)/.test(a)) return true;
    if (/^--(files-with-matches|files-without-match|count|quiet|silent)$/.test(a)) return true;
    // combined or single short flags: -l, -rl, -il, -c, -q, -L
    if (/^-[A-Za-z]+$/.test(a) && /[lLcq]/.test(a.slice(1))) return true;
  }
  return false;
}

/** Non-flag operands, excluding tokens consumed as flag values. */
function operands(args) {
  const out = [];
  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    if (a.startsWith('-')) {
      if (VALUE_FLAGS.has(a)) i++; // skip its value
      continue;
    }
    out.push(a);
  }
  return out;
}

function check(cmd) {
  // explicit opt-out
  if (/^\s*#\s*raw:/.test(cmd)) return null;

  for (const seg of segments(cmd)) {
    const t = tokenise(seg);
    if (!t.length) continue;

    // strip env assignments and common prefixes
    let k = 0;
    while (k < t.length && (/^[A-Z_][A-Z0-9_]*=/.test(t[k]) || ['sudo', 'command', 'nice', 'time'].includes(t[k]))) k++;
    const bin = (t[k] || '').replace(/^.*\//, '');
    const args = t.slice(k + 1);
    const rest = args.join(' ');

    // never touch write/heredoc forms or remote execution
    if (/[><]|<<|ssh\s|docker\s|kubectl\s/.test(seg)) continue;
    // downstream of a pipe: reading stdin, which is correct usage
    if (isDownstream(cmd, seg)) continue;

    const namesFile = args.some((a) => !a.startsWith('-') && /[./]|\.\w+$/.test(a));
    // a segment feeding another command never lands in context
    const consumed = pipesOut(cmd, seg);

    const CAP = 'Cap the output: add -l (names only), -c (count), -m N (first N matches), or pipe into `| head -n 50`.';

    if (bin === 'cat' && namesFile) {
      return {
        bin,
        use: 'Read',
        why: 'cat dumps the whole file into context permanently. Read takes offset/limit and is bounded.',
      };
    }
    if ((bin === 'head' || bin === 'tail') && namesFile) {
      return { bin, use: 'Read', why: 'Read with offset/limit does this without a shell round-trip.' };
    }
    if (bin === 'sed' && /-n\s*['"]?\d+\s*,\s*\d+p/.test(rest) && namesFile) {
      return { bin, use: 'Read', why: 'Reading a line range is exactly Read offset/limit.' };
    }

    // ---- search family: bound it, do not redirect it (no Grep/Glob here) ----
    if (consumed) continue;

    if (bin === 'grep' || bin === 'egrep' || bin === 'fgrep' || bin === 'rg' || bin === 'ag' || bin === 'ack') {
      const recursive = /(^|\s)(-[A-Za-z]*[rR][A-Za-z]*|--recursive)(\s|$)/.test(rest);
      const searchesPath = namesFile || operands(args).length >= 2 || recursive;
      if (searchesPath && !isBoundedSearch(args)) {
        return { bin, use: null, why: `An unbounded search dumps every match into context for the rest of the session. ${CAP}` };
      }
      continue;
    }
    if (bin === 'find' && !/-delete|-exec|-execdir|-ok|-quit/.test(rest)) {
      return { bin, use: null, why: 'find for discovery returns unbounded paths. Cap it: pipe into `| head -n 50`, or add -maxdepth and a -quit/-exec action.' };
    }
    if (bin === 'ls' && /(^|\s)(-[A-Za-z]*R[A-Za-z]*|--recursive)(\s|$)/.test(rest)) {
      return { bin, use: null, why: 'Recursive ls enumerates the whole tree into context. Cap it: pipe into `| head -n 50`.' };
    }
  }
  return null;
}

try {
  if (process.env.NATIVE_TOOL_HOOK === 'off') process.exit(ALLOW);

  const raw = readStdin();
  if (!raw.trim()) process.exit(ALLOW);

  let payload;
  try { payload = JSON.parse(raw); } catch { process.exit(ALLOW); }

  if (payload?.tool_name !== 'Bash') process.exit(ALLOW);
  const cmd = payload?.tool_input?.command;
  if (typeof cmd !== 'string' || !cmd.trim()) process.exit(ALLOW);

  const hit = check(cmd);
  if (!hit) process.exit(ALLOW);

  process.stderr.write(
    `Blocked: \`${hit.bin}\` via Bash.` + (hit.use ? ` Use the ${hit.use} tool instead.\n` : '\n') +
    `${hit.why}\n` +
    `If you genuinely need the unbounded shell form, prefix the command with "# raw:".\n`
  );
  process.exit(BLOCK);
} catch {
  process.exit(ALLOW); // fail open, always
}
