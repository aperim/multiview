// wave-fanout — launch one wave of lane work across DISJOINT file territories, each
// in its own isolated worktree, TDD-first, returning committed work + an opened PR for
// the orchestrator to review (review-wave) and merge. Territories must be disjoint and
// hot shared files (pipeline.rs, engine/{runtime,clock,drive}.rs,
// control/{routes/mod,openapi,state}.rs) must be assigned to a single owner lane —
// the orchestrator guarantees this in its ASSIGN step before calling. Usage:
//   Workflow({ name: 'wave-fanout', args: { lanes: [
//     { id: 'gpu-hwdefect', territory: 'LANE-GPU', item: 'HW-DEFECT-A', prompt: '...' , highRisk: false },
//     ... ] } })
export const meta = {
  name: 'wave-fanout',
  description: 'Run one wave of lane implementation across disjoint territories in parallel: each lane works TDD-first in an isolated worktree, runs the local gate, and opens a PR. Returns per-lane results (branch, commits, PR, gate status) for the orchestrator to review and merge. Disjoint-territory assignment is the orchestrator’s responsibility before calling.',
  whenToUse: 'The FAN OUT step of a Conductor wave once territories are assigned and dependency-ready.',
  phases: [{ title: 'Implement' }],
}

const LANE_RESULT = { type: 'object', additionalProperties: false,
  required: ['id', 'territory', 'status', 'branch', 'commits', 'localGate', 'summary'],
  properties: {
    id: { type: 'string' }, territory: { type: 'string' },
    status: { type: 'string', description: 'opened-pr | committed-no-pr | blocked | abandoned' },
    branch: { type: 'string' }, prNumber: { type: 'string' },
    commits: { type: 'array', items: { type: 'string' }, description: 'short SHAs in order, red test commit first' },
    localGate: { type: 'string', description: 'exact result of fmt/clippy/test/deny — paste pass/fail + any failing output' },
    filesTouched: { type: 'array', items: { type: 'string' } },
    summary: { type: 'string' }, blockers: { type: 'string' },
  } }

const RULES = 'NON-NEGOTIABLE: stay strictly within your territory paths; if you need a change to a hot shared file owned by another lane (pipeline.rs, engine/{runtime,clock,drive}.rs, control/{routes/mod,openapi,state}.rs) you do NOT own, implement only your body and describe the required wiring in blockers for the owner — do NOT edit it. TDD: write the failing test FIRST, run it, paste the actual red output, commit it as its own commit (Conventional Commits + the Co-Authored-By trailer), THEN implement to green without touching the test. Absolute typing, no unwrap/expect/panic in non-test code, no #[allow] without justification. Run the FULL local gate before opening the PR: cargo fmt --all -- --check; cargo clippy --workspace --all-targets -- -D warnings; cargo test --workspace (and web/ lint+typecheck+build if web/ changed; cargo deny check if deps changed). Build in your OWN isolated target/ (never /tmp). If the gate is not green, do NOT open the PR — return status=blocked with the failing output.'

function runLane(l) {
  return agent(
    `You own lane "${l.id}" (territory ${l.territory}) in the Multiview repo. Base your work on current origin/main (rebase if your worktree is on a stale base).\n\n` +
    `TASK: ${l.item}\n${l.prompt}\n\n` +
    `${RULES}\n\n` +
    `When green: push your branch and open a PR (\`gh pr create\`) with a Conventional-Commit title, a body explaining the change + how it was tested + the invariants re-asserted, ending with the "Generated with Claude Code" line. Return the structured result (branch, ordered commit SHAs, the PR number, the exact local-gate result, files touched, and any wiring you handed off to a hot-file owner in blockers). The orchestrator will run the cross-vendor review and own the PR to merge — do NOT merge it yourself.`,
    { label: `lane:${l.id}`, phase: 'Implement', schema: LANE_RESULT, isolation: 'worktree',
      ...(l.model ? { model: l.model } : {}), ...(l.highRisk ? { effort: 'high' } : {}) })
}

phase('Implement')
const lanes = (args && args.lanes) || []
const results = await parallel(lanes.map((l) => () => runLane(l)))
return { results: results.filter(Boolean) }
