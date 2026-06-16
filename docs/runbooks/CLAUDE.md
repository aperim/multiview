# docs/runbooks — operational runbooks

One runbook per provisioned resource or operational procedure. Written AS YOU
WORK (AGENTS.md rule 42): the same unit of work that provisions or changes a
resource (a database, queue, bucket, namespace, worker/service, scoped token,
DNS/route, CI environment/secret, local dev service) creates or updates that
resource's runbook here **in the same commit**.

Each runbook captures:

- what it is and **why** it exists,
- the exact command/API call used to create or change it,
- the resource id / name / binding,
- how to **verify** it,
- how to **rotate / recreate / restore / roll back**.

Present tense, executable, kept current — never aspirational (rule 27: a runbook
states what _is_, updated the moment reality changes). Runbooks are the **how**;
ADRs in [`../decisions/`](../decisions/) are the **why**.

## Relationship to `docs/operations/`

[`docs/operations/`](../operations/) holds broader operational *guides*
(building, containerization, devcontainer, observability, testing &
benchmarking) — narrative how-to for the whole system. `docs/runbooks/` holds
*resource-scoped* runbooks for specific provisioned things (e.g. the local
memory MCP, a CI secret, a deployed service). When a runbook grows into general
guidance, link it from the relevant `docs/operations/` guide rather than
duplicating.
