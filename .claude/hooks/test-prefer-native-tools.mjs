#!/usr/bin/env node
/**
 * Regression test for prefer-native-tools.mjs.
 *
 * The hook can block every shell call in the repo, so it is gated like a safety
 * control: the repo's own commands MUST keep working, and the unbounded forms
 * MUST keep being caught. Run:  node .claude/hooks/test-prefer-native-tools.mjs
 *
 * The load-bearing constraint, from the 2026-07-27 harness check: `Grep` and
 * `Glob` do NOT exist here, so a block may never tell the agent to use them.
 * Every block must name an action the agent can actually take — bound the
 * search, or use `Read`.
 */

import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const HOOK = join(dirname(fileURLToPath(import.meta.url)), 'prefer-native-tools.mjs');

/** @returns {{blocked: boolean, stderr: string}} */
function run(command, env = {}) {
  const r = spawnSync('node', [HOOK], {
    input: JSON.stringify({ tool_name: 'Bash', tool_input: { command } }),
    encoding: 'utf8',
    env: { ...process.env, ...env },
  });
  if (r.status !== 0 && r.status !== 2) throw new Error(`hook crashed (${r.status}) on: ${command}\n${r.stderr}`);
  return { blocked: r.status === 2, stderr: r.stderr || '' };
}

const MUST_BLOCK = [
  // unbounded searches — the thing this hook exists to stop
  ['rg "pattern" src', 'search with no cap'],
  ['rg -n "ADR-T003" docs/', 'content search with no cap'],
  ['grep -rn "invariant" docs/', 'recursive grep'],
  ['grep -r TODO crates/', 'recursive grep, no path arg pattern'],
  ['ag "foo" src', 'the silver searcher'],
  ['find . -name "*.rs"', 'unbounded find'],
  ["find . -name '*.ts'", 'unbounded find, single quotes'],
  ['ls -R crates/', 'recursive ls'],
  // whole-file reads — Read exists, so these always have a remedy
  ['cat AGENTS.md', 'cat a file'],
  ['cat crates/multiview-core/src/lib.rs', 'cat a source file'],
  ['head -50 Cargo.toml', 'head a file'],
  ['tail -20 CHANGELOG.md', 'tail a file'],
  ["sed -n '10,40p' AGENTS.md", 'sed line range'],
];

const MUST_ALLOW = [
  // bounded searches
  ['rg -l "pattern" src', 'names only'],
  ['rg -m5 "pattern" src', 'attached max-count'],
  ['rg -m 5 "pattern" src', 'detached max-count'],
  ['rg --max-count=5 "pattern" src', 'long max-count'],
  ['rg -c "pattern" src', 'counts'],
  ['rg --files-with-matches "pattern" src', 'long names-only'],
  ['rg --type rust -l "AVHWFramesContext" crates/multiview-ffmpeg', 'typed + bounded'],
  ['grep -c foo file.txt', 'grep count'],
  ['grep -rl TODO crates/', 'recursive but names-only'],
  ['grep -q foo file.txt', 'quiet'],
  // output consumed by a pipe never reaches the transcript
  ['rg "pattern" src | head -n 50', 'piped into head'],
  ['grep "${tarball}" checksums.txt | sha256sum -c -', 'gitleaks checksum verify (.github/workflows/gitleaks.yml)'],
  ["ls docs/decisions/ | grep '^ADR-G'", 'the documented next-ADR-number idiom'],
  ["find . -name '*.rs' | head -20", 'piped find'],
  ['grep -rn "invariant" docs/ | wc -l', 'piped into wc'],
  ['ls -R crates/ | head -n 20', 'piped recursive ls'],
  // find with an action, and redirects
  ['find . -name "*.tmp" -delete', 'find with an action'],
  ['git diff origin/main...HEAD > /tmp/review.diff', 'redirect'],
  ["cat \"$LOCK\" 2>/dev/null || echo ''", 'tick.sh lock read (has a redirect)'],
  // the repo's real contract commands must never be touched
  ['scripts/classify.sh', 'AGENTS.md'],
  ['cargo check --workspace', 'AGENTS.md'],
  ['cargo test --workspace', 'AGENTS.md'],
  ['cargo fmt --all -- --check && cargo clippy --locked --workspace --all-targets -- -D warnings', 'AGENTS.md gate'],
  ['npm --prefix web ci && npm --prefix web run lint && npm --prefix web run build', 'AGENTS.md web gate'],
  ['git log --oneline -20', 'AGENTS.md working-efficiently'],
  ['git worktree list', 'worktree-lane skill'],
  ['git -C "$ROOT" worktree add --detach ".claude/worktrees/$LANE" HEAD', 'worktree-lane skill'],
  ['gh pr list --state open --json number,title,statusCheckRollup,mergeable', 'orchestrate skill'],
  ['node .claude/skills/orchestrate/lib/test.mjs', 'orchestrate lib'],
  ['./.claude/skills/orchestrate/tick.sh', 'the scheduler entry point'],
  ['ls crates/', 'plain non-recursive ls'],
];

let pass = 0;
const fail = [];

for (const [cmd, why] of MUST_BLOCK) {
  const { blocked, stderr } = run(cmd);
  if (!blocked) { fail.push(`should BLOCK (${why}): ${cmd}`); continue; }
  // every block must offer an action that exists in this harness
  if (/\bGlob\b/.test(stderr) || /Use the Grep tool/.test(stderr)) {
    fail.push(`block names a non-existent tool (${why}): ${cmd}\n    ${stderr.split('\n')[0]}`);
    continue;
  }
  if (!/Read tool|Cap the output|Cap it|# raw:/.test(stderr)) {
    fail.push(`block offers no actionable remedy (${why}): ${cmd}`);
    continue;
  }
  pass++;
}

for (const [cmd, why] of MUST_ALLOW) {
  const { blocked, stderr } = run(cmd);
  if (blocked) { fail.push(`should ALLOW (${why}): ${cmd}\n    ${stderr.split('\n')[0]}`); continue; }
  pass++;
}

// escape hatches
for (const [cmd, label] of [['# raw: rg "x" src', '# raw: prefix'], ['# raw: cat AGENTS.md', '# raw: on cat']]) {
  if (run(cmd).blocked) fail.push(`escape hatch broken (${label}): ${cmd}`); else pass++;
}
if (run('cat AGENTS.md', { NATIVE_TOOL_HOOK: 'off' }).blocked) fail.push('NATIVE_TOOL_HOOK=off did not disable the hook'); else pass++;

// fail-open contract: junk input must never block
for (const bad of ['', 'not json', '{}', '{"tool_name":"Read"}', '{"tool_name":"Bash"}', '{"tool_name":"Bash","tool_input":{}}']) {
  const r = spawnSync('node', [HOOK], { input: bad, encoding: 'utf8' });
  if (r.status !== 0) fail.push(`must fail OPEN on malformed payload (exit ${r.status}): ${JSON.stringify(bad)}`); else pass++;
}

const total = pass + fail.length;
if (fail.length) {
  console.error(`\n${fail.length} FAILED of ${total}:\n`);
  for (const f of fail) console.error('  ✗ ' + f);
  process.exit(1);
}
console.log(`${pass}/${total} passed`);
