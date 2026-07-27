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
 * all, so only the tail of a pipeline is checked — including for file reads,
 * because `Read` cannot feed a pipe and `head -100 build.log | grep error` has
 * no Read-shaped remedy.
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

/**
 * Split a command line into segments, ignoring separators inside quotes, and
 * record the separator on each side of every segment.
 *
 * The separators are carried on the segment rather than recovered later by
 * searching the command string: `rg foo src | head -5 && rg foo src` contains
 * the same segment text twice, and an indexOf-style lookup resolves both to the
 * first occurrence — under-blocking the second (unbounded) one, and
 * over-blocking the reverse arrangement.
 *
 * `||` and `&&` are recorded as 'logical', never as a pipe: `foo || cat f`
 * does not feed `cat` anything.
 */
function segments(cmd) {
  const out = [];
  let buf = '';
  let quote = null;
  let sepBefore = null;
  for (let i = 0; i < cmd.length; i++) {
    const c = cmd[i];
    if (quote) {
      if (c === quote && cmd[i - 1] !== '\\') quote = null;
      buf += c;
      continue;
    }
    if (c === '"' || c === "'") { quote = c; buf += c; continue; }
    if (c === '|' || c === ';' || c === '&') {
      let sep = c;
      // a doubled operator is control flow, not a pipe
      while (i + 1 < cmd.length && (cmd[i + 1] === '|' || cmd[i + 1] === '&')) { i++; sep = 'logical'; }
      if (buf.trim()) out.push({ text: buf.trim(), sepBefore, sepAfter: sep });
      buf = '';
      sepBefore = sep;
      continue;
    }
    buf += c;
  }
  if (buf.trim()) out.push({ text: buf.trim(), sepBefore, sepAfter: null });
  return out;
}

/** True when the segment reads piped stdin rather than naming files. */
const isDownstream = (seg) => seg.sepBefore === '|';

/**
 * True when this segment's stdout is piped onward. Its bytes are consumed by
 * the next stage and never enter the transcript, so output size is not our
 * problem — only the tail of a pipeline reaches context.
 */
const pipesOut = (seg) => seg.sepAfter === '|';

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

/**
 * Bounded search: -l / -c / -q (incl. combined shorts) or an explicit
 * -m/--max-count.
 *
 * `-L` is bounded in grep and ag (files-WITHOUT-match) but in ripgrep it is
 * --follow, which *widens* the search — so it counts only for the grep family.
 */
function isBoundedSearch(bin, args) {
  const shorts = bin === 'rg' ? 'lcq' : 'lLcq';
  const re = new RegExp(`[${shorts}]`);
  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    if (a === '-m' || a === '--max-count' || /^-m\d+$/.test(a)) return true;
    if (/^--max-count(=|$)/.test(a)) return true;
    if (/^--(files-with-matches|files-without-match|count|quiet|silent)$/.test(a)) return true;
    // combined or single short flags: -l, -rl, -il, -c, -q
    if (/^-[A-Za-z]+$/.test(a) && re.test(a.slice(1))) return true;
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

  for (const segment of segments(cmd)) {
    const seg = segment.text;
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
    if (isDownstream(segment)) continue;
    // feeding another command: these bytes never reach the transcript, and
    // `Read` cannot feed a pipe, so blocking here would offer no remedy
    if (pipesOut(segment)) continue;

    const namesFile = args.some((a) => !a.startsWith('-') && /[./]|\.\w+$/.test(a));

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
    if (bin === 'grep' || bin === 'egrep' || bin === 'fgrep' || bin === 'rg' || bin === 'ag' || bin === 'ack') {
      const recursive = /(^|\s)(-[A-Za-z]*[rR][A-Za-z]*|--recursive)(\s|$)/.test(rest);
      const searchesPath = namesFile || operands(args).length >= 2 || recursive;
      if (searchesPath && !isBoundedSearch(bin, args)) {
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
