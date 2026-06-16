// review-wave — rule-21 adversarial cross-vendor review of one or more diffs/PRs.
// Each Claude-authored diff is reviewed by a DIFFERENT vendor (Codex) in a fresh
// context seeing only the diff + spec + checklist. High-risk items get a 3-reviewer
// panel (Codex + two fresh-context lenses). Returns structured verdicts; the
// orchestrator gates merge on them. Usage:
//   Workflow({ name: 'review-wave', args: { items: [
//     { id: 'pr-170', ref: '170', spec: 'webrtc ICE concrete local addr; invariant #10 isolation', highRisk: false },
//     { id: 'pr-172', ref: '172', spec: 'output-clock cadence-hold; invariant #1', highRisk: true } ] } })
// ref = a PR number (reviewed via `gh pr diff <n>`) or a git ref (`git diff origin/main...<ref>`).
export const meta = {
  name: 'review-wave',
  description: 'Rule-21 adversarial cross-vendor review: dispatch each Claude-authored diff to Codex (a different vendor) in a fresh context with only diff+spec+checklist; high-risk diffs get a 3-reviewer panel. Returns per-item verdicts (defects + >=1 residual risk + blocked?) for the orchestrator to gate merge on.',
  whenToUse: 'Before merging any lane/PR — the mandatory review gate (rule 21, ADR-G005). Never self-performed by the authoring vendor.',
  phases: [{ title: 'Review' }],
}

const VERDICT = { type: 'object', additionalProperties: false,
  required: ['id', 'reviewer', 'ranOk', 'defects', 'highestResidualRisk', 'blocked'],
  properties: {
    id: { type: 'string' }, reviewer: { type: 'string', description: 'codex | codex+panel | claude-fallback — the vendor whose judgment this is' },
    ranOk: { type: 'boolean', description: 'false if the reviewer CLI could not be run; then this is a labeled fallback' },
    defects: { type: 'array', items: { type: 'object', additionalProperties: false, required: ['severity', 'where', 'what'],
      properties: { severity: { type: 'string', description: 'blocker | major | minor' }, where: { type: 'string', description: 'file:line' }, what: { type: 'string' } } } },
    highestResidualRisk: { type: 'string', description: 'the single highest-residual-risk area — required even when no defect found (rule 16)' },
    blocked: { type: 'boolean', description: 'true if any blocker/major defect must be fixed before merge' },
  } }

const CHECKLIST = 'Scope (agent-guardrails.md §C): correctness, security, spec-conformance, and the typing/TDD guardrails ONLY. NOT style/naming/speculative hardening. Multiview invariants to weigh: #1 output-clock (one valid frame per tick, inputs never pace), #10 isolation (control/preview/realtime physically cannot back-pressure the engine), bounded drop-oldest queues, no unwrap/panic on the hot path, FFI safety. Report concrete defects as file:line + severity; if none, name the single highest-residual-risk area. Unanimous bland approval is a yellow flag — always surface >=1 substantive risk.'

function reviewItem(it) {
  const single = (lensNote, idSuffix) => agent(
    `You are the harness for a CROSS-VENDOR adversarial code review. The authoring vendor is Claude; the REVIEWER must be a DIFFERENT vendor — the \`codex\` CLI (codex-cli, installed). Do NOT substitute your own Claude judgment for the verdict; run Codex and relay ITS findings faithfully.\n\n` +
    `Steps (cwd /workspaces/mosaic):\n` +
    `1. Build the diff. If ref "${it.ref}" is all digits it is a PR: \`gh pr diff ${it.ref} > /tmp/rv-${it.id}${idSuffix}.diff\`. Else: \`git diff origin/main...${it.ref} > /tmp/rv-${it.id}${idSuffix}.diff\`.\n` +
    `2. Discover the right non-interactive Codex invocation: \`codex --help\` then \`codex exec --help\` (or \`codex e --help\`). Use the read-only/non-interactive exec form (e.g. \`codex exec --sandbox read-only "<prompt>"\`). The prompt must tell Codex to read /tmp/rv-${it.id}${idSuffix}.diff and review it.\n` +
    `3. Prompt Codex: "Adversarially review this diff. Spec: ${it.spec}. ${lensNote} ${CHECKLIST}". Capture Codex's full output (give it a generous timeout).\n` +
    `4. If Codex ran, set reviewer="codex"${idSuffix ? ' (lens: ' + lensNote + ')' : ''}, ranOk=true, and STRUCTURE CODEX'S findings into the verdict (do not invent defects Codex did not raise; do not drop ones it did). If Codex could NOT be run after a genuine attempt (report the error), fall back to performing the review yourself in this fresh context, set reviewer="claude-fallback", ranOk=false, set blocked=true (a fallback is not cross-vendor and must fail closed), and note the error in highestResidualRisk.\n` +
    `Return the structured verdict for id "${it.id}${idSuffix}".`,
    { label: `review:${it.id}${idSuffix}`, phase: 'Review', schema: VERDICT })

  if (!it.highRisk) return single('', '')
  // High-risk: 3-reviewer panel — Codex + two fresh-context lenses — then aggregate.
  return parallel([
    () => single('Lens: correctness & concurrency/TOCTOU/race/timing.', '-a'),
    () => single('Lens: security & authorization (BOLA/per-object authz) & input hardening.', '-b'),
    () => single('Lens: spec-conformance & the Multiview invariants (#1 output-clock, #10 isolation) & data-plane bounded-queue discipline.', '-c'),
  ]).then((panel) => agent(
    `Aggregate this 3-reviewer panel for high-risk item "${it.id}" into one verdict. A defect raised by ANY reviewer stands. blocked = true if any reviewer flagged a blocker/major. reviewer="codex+panel". Panel:\n${JSON.stringify(panel.filter(Boolean))}`,
    { label: `panel:${it.id}`, phase: 'Review', schema: VERDICT }))
}

phase('Review')
const items = (args && args.items) || []
const verdicts = await pipeline(items, reviewItem)
// Fail closed: a fallback verdict (ranOk=false → not actually cross-vendor) can NEVER
// clear the merge gate, regardless of whether the fallback reviewer found a defect.
// The gate keys off `blocked`, so couple it to ranOk here rather than trusting prose
// (codex-review runbook + orchestrate skill: "never merge on a fallback verdict").
const enforced = verdicts.filter(Boolean).map((v) => (v.ranOk === false ? { ...v, blocked: true } : v))
return { verdicts: enforced }
