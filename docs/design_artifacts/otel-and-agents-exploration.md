# Exploration: OpenTelemetry, and where agents fit in the ontology

- **Status:** exploration, pre-ADR. Nothing here is decided.
- **Date:** 2026-07-31
- **How to read it:** Part 1 and Part 2 are independent proposals that happen to
  share one idea, stated in §0. Each part ends with variations ordered
  conservative → radical, a recommendation, and the things I would refuse to build.

Terms are [CONTEXT.md](../../CONTEXT.md)'s. Where a proposal strains a term or
contradicts an accepted ADR, it is called out by name rather than papered over.

---

## §0 The one idea both halves share

Both features are usually built as *new subsystems*. Here they are both **exports of
a record we already keep**:

- A **trace** is not something we emit while working — it is a **projection of the
  event log**, the same way the timeline, SSE and audit views already are.
- An **agent**'s durability is not a new execution model — it is **the existing
  at-least-once contract at a finer grain**, with the carried state in the data
  plane instead of a process stack.

Every proposal below that respects this is cheap. Every proposal that doesn't
(hold a span open for three weeks; replay agent code deterministically) is expensive
*and* contradicts an accepted ADR. That correlation is not a coincidence.

---

# Part 1 — OpenTelemetry, outside and inside Steps

## 1.1 Ground truth (verified, with anchors)

Worth stating precisely, because three of these facts kill otherwise-attractive
designs.

- **No OTel anywhere.** `opentelemetry`/`otlp`/`traceparent`/`b3` appear nowhere in
  `crates/`, any `Cargo.toml`, or `Cargo.lock`. The only hits in the tree are
  [ADR-0053](../adr/0053-observability-and-lifecycle.md) deferring it. Greenfield.
- **Logging is in decent shape.** JSON by default with `SCARAB_LOG_FORMAT=text` as
  the opt-out, `EnvFilter` honoured (`scarab-server/src/main.rs:43-56`), and a
  global request-id middleware (`scarab-server/src/lib.rs:7351-7370`, layered at
  `:7781`). One real defect: the middleware holds a *synchronous* span guard across
  an `.await` (`lib.rs:7362-7365`) instead of using `.instrument()`, so the header
  correlates but the log lines do not reliably. Fix that first; it is ten lines.
- **`#[instrument]` count is zero, tree-wide.** There is no span tree beyond that
  one `info_span!`. `tracing::` call sites: `scarab-server` 92, `scarab-storage` 11,
  `scarab-executor-k8s` 4, **`scarab-engine` 0, `scarab-db-postgres` 0**. The dark
  crates are dark *deliberately* — the pure engine emits no logs and the driver
  surfaces them (`converged.rs:47-50`) — which is right for purity and means the
  entire state machine is unobservable from outside the process.
- **Metrics are hand-rolled and have no latency at all.** No `prometheus`/`metrics`
  crate anywhere; `/metrics` is string formatting over `AtomicU64`
  (`scarab-server/src/metrics.rs`, `lib.rs:7297-7330`, `workspaced.rs:2162-2199`).
  **11 series, zero histograms, zero durations.** ADR-0053's promised admission
  latency, steps-by-status, log-tail lag and GC counters were never built. The
  `ServiceMonitor` exists but is `enabled: false` by default
  (`deploy/helm/scarab/values.yaml:343-345`) and the workspace StatefulSet has none
  at all — so in every shipped configuration its 6 series are unscraped.
- **The tick is unmeasured.** 500 ms hardcoded at one call site
  (`main.rs:521`), sleep-*after*-work so the real period drifts under load
  (`converged.rs:177-198`), and **no `Instant::now()` anywhere in the engine or the
  driver**. A clean tick and a tick that has silently slowed to 30 s are
  indistinguishable from outside the process.
- ⚠ **`attempts` has `started_at` and no `finished_at`** (`0001_initial.sql:39`;
  later migrations add results/outcome, never an end time). A span needs an end, so
  **the Run trace must be projected from `events`**, where `AttemptFinished` carries
  the instant. `events.at` is `BIGINT` unix **millis** (mirroring the domain's
  `Timestamp(i64)`), so **the projected Run trace has 1 ms resolution, permanently.**
- ⚠ **`GET /v1/runs/{id}/events` is not a stream.** It does one `db.events(&run)`
  and returns `Sse::new(stream::iter(items))`, which closes immediately
  (`lib.rs:1727-1746`) — no live tail, no `Last-Event-ID`, and the `events.seq`
  column unused. Meanwhile `/logs` live-tails by **polling Postgres + the object
  store every 500 ms per connection** (`lib.rs:1794-1834`), re-reading the whole
  run's chunk index each time, and the in-process `broadcast` channel built in
  `logs.rs` is effectively dead on the HTTP path.
- **The sidecar is a shell script whose failures are invisible.**
  `docker/sidecar/scarab-results-egress.sh` idles then drains `*.json` over `curl`.
  Its stderr is **not collected** — `log_stream` pins the tail to the `step`
  container (`executor-k8s/src/lib.rs:1666-1667`) and `ensure_log_tails` enumerates
  only `service-{i}` (`converged.rs:98-106`). A "POST failed after retries" is
  invisible to Scarab, to the log UI, and to metrics. The ingest handler
  (`lib.rs:5503-5555`) emits **no `tracing` event on any path**, so a 401 storm from
  a skewed token is silent too.

## 1.2 Four constraints that are not negotiable

1. **ADR-0053 chose Prometheus pull** and deferred OTel. Nothing above needs
   replacing; OTel *metrics* would be a lateral move. Traces are the gap.
2. **No DaemonSet.** [ADR-0062](../adr/0062-workspace-export-lazy-without-node-driver.md)
   spent real design effort avoiding a node driver. That single constraint kills
   node-level eBPF, a Collector DaemonSet, and Fluent Bit/Vector. Only *per-Pod*
   containers and kubelet-arranged volumes remain.
3. **ADR-0039's restricted floor.** `CAP_BPF`/`CAP_PERFMON` are `add-capabilities` —
   a **governed**, digest-pinned, Administer-only grant. In-Pod eBPF is therefore
   architecturally possible and can *never* be the default path.
4. **ADR-0063's doctrine.** Step logs are on the Data Depot, Postgres holds offsets,
   and external log systems are "**additional sinks, never the system of record**".
   Part 1 must obey this, and §1.6/O4 is where the temptation to break it lives.

## 1.3 The three hard problems

### (a) A trace cannot be held in memory — so make identity *derived*, not propagated

An OTel SDK span is a live object owned by a process. Every assumption there is
false for a Run: it can park on a `gate` for three weeks, move between replicas, and
survive the death of whatever "started" it. So the instrumentation-first approach
(sprinkle `#[instrument]`, wire `tracing-opentelemetry`) yields correct traces of
*control-plane operations* and **structurally cannot** yield a Run trace.

Compute IDs from durable identity instead of carrying them:

```
trace_id = HMAC-BLAKE3(K, "scarab.trace.v1|" || run || "|" || take)[0..16]
span_id  = HMAC-BLAKE3(K, "scarab.span.v1|"  || run || "|" || step
                          || "|" || attempt || "|" || sub)[0..8]
```

`K` is `SCARAB_TRACE_ID_KEY` so span IDs are not a run-id oracle; the `v1|` prefix
makes the derivation versionable — **do that on day one**, because the derivation is
a compatibility surface the moment anything external stores a correlation.

What falls out is better than the problem it solves:

- **Emission is idempotent.** Any replica can (re-)emit any span; duplicates are
  byte-identical, so at-least-once delivery through the **existing outbox** is
  sufficient. This is the durability wedge applied to telemetry: *your traces
  survive the crash your build survived.* No CI ships this.
- **There is no context-propagation problem.** A Step Pod's `TRACEPARENT` is
  computable from `{run, take, step, attempt}` before the parent span exists — the
  same trick as ADR-0034's gate HMAC, which is also "a pure function of the fence and
  a secret, no storage". The UI can show a trace link the instant a Run is created.
- ⭐ **The fence token *is* the span ID.** `{run, step, attempt}` is already the
  fencing unit, already stamped into `SCARAB_RUN/STEP/ATTEMPT`
  (`executor-k8s/src/lib.rs:2436-2439`), already the idempotency key handed to
  cooperating external systems. Under this derivation it is also the span ID in a
  different encoding — so a deploy target that recorded nothing but a Scarab
  idempotency key six months ago is **retroactively joinable to the trace with zero
  coordination**. Fencing and tracing turn out to be the same identifier problem;
  nobody proposes this because one is filed under correctness and the other under
  observability.

One column is genuinely needed: `outbox.trace_context` (the W3C `traceparent` of the
enqueuing span), because the enqueuing process may be dead when the row is drained.
That is the standard propagate-through-a-queue pattern and it is unavoidable.

**This is validated prior art, not a novel gamble** — which is the best news in Part 1.
The OTel Collector's `githubreceiver` derives its IDs exactly this way:
`trace_id = sha256("{run_id}{run_attempt}t")[0:32]`, step span
`= sha256("{check_run_id}-{step_name}-s")[16:32]`. Note `run_attempt` sits *inside the
trace ID* — the same call we make by putting `take` there. Honeycomb's `buildevents`
likewise derives the trace ID from the CI build ID and emits backdated spans. The
contrast worth citing in the ADR is **Tekton**, which chose the other branch: it
persists span context as annotations (`tekton/pipelinerun-span-context`) to survive
reconcile loops — storage, plus a context that can be lost, to buy what derivation
gives for free.

### (b) The backend will drop your three-week span

The numbers are worse than "weeks is too long", and they are documented:
**Datadog accepts spans up to 18 h in the past / 2 h in the future**, full stop.
**Jaeger on ES** composes a bounded list of daily indices from `--es.max-span-age`,
**default 72 h** — and raising it past ~4 months overflows ES's 4096-byte HTTP line
limit, so the trace becomes *unfindable by ID*. **Tempo** has `max_trace_live=30s`,
`complete_block_timeout=20m`; fragments land in different blocks, TraceQL duration is
computed from a subset, and spanset operators only evaluate the contiguous trace in
the current block — **Tempo's own documentation recommends "using span links to
intentionally split traces."** Grafana's metrics-generator has 30 s of ingestion-time
slack. And the OTel spec has **no adopted solution**: the API requires every span to
end, exporters only see ended spans, and the sanctioned pattern is one span per phase
joined by links. AWS X-Ray's `in_progress: true` segment is the only first-class
long-operation primitive in any backend, and we are not building for X-Ray.

So a Run **must not be one span** whose duration is its wall-clock life. State it as
an invariant:

> **No emitted span may be longer than the configured ingest window** (default 6 h).
> Anything longer is segmented, and the seam is an explicit span link.

The seam already exists in the domain: **a durable suspend is a trace boundary.** The
Run root closes at gate entry with a `scarab.gate.parked` event; the release opens a
new root linked back with `scarab.link.kind = resumed_from`. A parked Run emits **no
spans at all** while parked — it emits a `scarab_run_parked_seconds` gauge, which is
what an operator actually wants. Heartbeat spans are theatre; reject them. Refused
too-old spans are **counted loudly**
(`scarab_trace_spans_dropped_total{reason="too_old"}`) — ADR-0063's rule that absence
must be visible, applied here.

**Segmentation is what the two vendors who actually confronted this chose**, which is
worth knowing before we invent:

- **Datadog CI Visibility**: "pipeline executions must end before sending them"; a
  blocked pipeline gets `status: blocked`, and **the resumed portion is a different
  pipeline ID with `is_resumed: true`**; partial retries get a new ID plus
  `partial_retry: true`. Their `is_resumed`/`partial_retry` are prior art for our
  `resumed_from`/`rerun_of` links.
- **Buildkite** emits **an extra `buildkite.build.stage` span per period the build
  spends running** — a block/resume becomes several stage spans, not one long one.
- **Jenkins** does the opposite and shows why: a paused build is *one unbounded span*
  in state `PAUSED_PENDING_INPUT`, exported only at run end. An `input`-gated Jenkins
  build is exactly the span every backend above will refuse.

⭐ **And then the good idea: emit a second, active-time trace.** At Run terminal,
emit a companion trace on a synthetic timebase anchored at the terminal instant:
gates collapse to zero-duration events carrying `scarab.gate.wall_ms`, and every
span's start is its cumulative *active* offset. This is exactly the "billed active
time only" computation that already exists for the ADR-0047 run budget
(`scheduler.rs:2494`). It is what a pipeline author actually wants — a critical path
with the human waiting removed — and because it is anchored at the end it **always
fits inside every backend's ingest window, including for a Run parked three weeks**.
Tag `scarab.trace.kind = active | wallclock`, link them mutually.
**Red line:** its timestamps are not wall-clock and must never be correlated with
another system's traces. Say so in the attribute *and* the docs.

### (c) Takes, Reruns, Superseded — what is one trace?

**A Take is a trace**, because a Take is "the run-level version unit" and the thing a
human means by "this run". `trace_id` includes `take`, so Take 2 is a new trace, not
a re-parenting.

- A **Rerun** links Take 2's root back with `scarab.link.kind = rerun_of`; each
  cascaded attempt links to the exact **superseded** attempt it displaced — and the
  link is **free**, because the displaced span's ID is derivable without storing
  anything. ADR-0056's version lattice reconstructs itself from deterministic IDs.
- **Auto-retry** and manual **Retry** stay *inside* the Take's trace as sibling
  attempt spans. A retry is not a new version.
- **Superseded is not span status `Error`.** It is `scarab.attempt.outcome =
  superseded` plus a `scarab.superseded{by}` span event. ADR-0056 is explicit that
  superseded is an honest terminal fate, not a failure; painting it red is exactly the
  repainting the ADR forbids. `shadowed` is a boolean on a green span, never a status.
- **Not-run steps emit no span.** A zero-duration span would imply something happened.

## 1.4 Three customers, three signal shapes

Conflating these is why most CI observability is useless.

| Customer | Question | Right primitive |
|---|---|---|
| **Scarab operator** | is my engine healthy? | Prometheus pull (keep 0053) **plus the histograms that were never built**, exemplars into traces, and process-local spans on API/sqlx/kube/forge/Depot. Not the Run trace. |
| **Pipeline author** | why is my build slow or flaky? | the **active-time Run trace** + test spans + the cgroup profile. Wants engine internals *hidden*. |
| **Platform team** | fleet cost, queue time, contention | metrics + span-metrics aggregation downstream. |

Emit author-facing spans under `service.name = scarab.pipeline` and operator spans
under `scarab.engine`, so a UI can collapse one. **Cardinality guardrail:**
`vcs.ref.head.revision` and `cicd.pipeline.run.id` are span-only attributes and must
never become metric labels.

The author row is where the differentiation is, and it is not "we support OTel" — it
is the **phase attribution only Scarab can produce**:

```
run 7f3a  ██████████████████████████████████████  14m02s
  admission (queued)     ▏ 3.1s    scarab.admission.group=deploy
  step clone             ██ 6.4s
    export mount         ▏ 0.4s    scarab.export.kind=overlayfs
    fetch snapshot       ▏ 1.2s    scarab.depot.tier=warm  hit_ratio=0.94
  step test              ████████████████████████████  11m48s
    image pull           ███ 41s
    command              █████████████████████████ 11m02s
    drain (change set)   ▏ 18s     scarab.changeset.paths=412
```

GitHub Actions structurally cannot give you the queue/materialise/drain split. We
know all of it durably already; the trace is a rendering.

**Semantic conventions — better news than expected.** The `cicd.*` conventions are
**Release Candidate**, promoted in semantic-conventions **v1.42.0 (2026-06-12)** after
landing experimental in v1.27.0. Spans, metrics, logs, attributes and entities are all
defined, so we can be conformant rather than inventive:

- **Spans are deliberately two levels only:** a pipeline-run span (kind `SERVER`, name
  `{cicd.pipeline.action.name} {cicd.pipeline.name}`, required `cicd.pipeline.result` ∈
  `success|failure|timeout|skip|error|cancellation`) and a pipeline-task-run span (kind
  `INTERNAL`, required `cicd.pipeline.task.name`, `.task.run.id`, `.task.run.result`,
  `.task.run.url.full`). **There is no step-level or worker-level span, and no attempt
  or take grain** — so `scarab.attempt.*` and `scarab.take` are ours, and worth taking
  to the CI/CD SIG rather than diverging quietly. v1.41.0 clarified that
  `cicd.pipeline.task.run.id` must be unique within a pipeline run, which our fence
  satisfies for free.
- **Resources:** `cicd.pipeline`, `cicd.pipeline.run`, `cicd.worker`, `vcs.repository`
  (all RC) and `vcs.ref` (still Development). Note the spec says the pipeline-run
  resource implies **a TracerProvider per run** — which is a hint that the SDK model
  is the wrong tool and O2's hand-emission is the right one — and that attaching it to
  *metrics* **MUST be opt-in** for cardinality. That is §1.4's guardrail, in the spec.
- **`test.*` is much weaker:** four keys, all Development stability (`test.case.name`,
  `test.case.result.status`, `test.suite.name`, `test.suite.run.status`). Fine for O3,
  but expect churn.
- **Env-var context propagation is now a Release Candidate spec** ("Environment
  Variables as Context Propagation Carriers", from OTEP #258): keys must match
  `^[A-Z_][A-Z0-9_]*$`, `TRACEPARENT`/`TRACESTATE` are the de-facto names, and it
  recommends treating them as read-only startup input. O3's injection is sanctioned.

## 1.5 Inside a Step: the menu, ranked by honesty

| Mechanism | Verdict |
|---|---|
| ⭐ **Step-local OTLP on the sidecar** + injected `TRACEPARENT` / `OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318` | **Do this first.** Same Pod = same netns, so localhost works while the step keeps *zero* external egress. The sidecar buffers to the shared `emptyDir` and drains with the same acked-retry loop as results — a literal instantiation of ADR-0042 §5 ("a general egress mechanism, results-first"). It **stamps `{org, project, run, take, step, attempt, actor}` as resource attributes the step cannot forge** — the *trusted* half of trusted egress applied to telemetry. And because the parent ID is derived, user spans are parented correctly even before the control plane emits the parent. |
| **Test-level spans from JUnit XML** converted by the sidecar (`test.case.name`, `test.case.result.status`) | **Best value/effort inside a step.** The timings are the framework's own; no kernel anything. Directly answers "why is my build flaky". |
| **Per-Attempt cgroup v2 profile** (`memory.peak`, `cpu.stat`, `io.stat`, `pids.peak`) | **Honest, one read, no privilege.** Settles OOM and IO-bound questions, and funds a real feature: *"this step requests 4Gi; peak was 780Mi over 20 runs."* **Doctrinal point: this is evidence**, so it belongs Attempt-keyed on the Depot with OTel as a sink — otherwise we have made an external system the system of record. |
| **Process-tree spans** (two mechanisms, see below) | **Conditionally honest.** Read the security finding first. |
| **eBPF syscall spans** | **Near-unbuildable here and mostly theatre.** No-DaemonSet kills the node agent; in-Pod needs a governed digest-pinned grant, so it exists only on whitelisted Environments. And 10⁵ syscall spans is not observability. Legitimate as an opt-in *debugging profile*, never as instrumentation. |
| **LD_PRELOAD interposition** | **Theatre.** Breaks static binaries, Go, musl/glibc mixes — silently. A CI that corrupts a build to observe it has failed. |
| **Network egress map** | **Not available honestly.** conntrack/flow capture needs privilege or a CNI we don't own. The honest version is an explicit egress **proxy** steps are pointed at — a product decision, not instrumentation. (Part 2 §2.7 wants exactly that proxy for other reasons.) |

**Two ways to get the process tree, and neither is eBPF:**

- ⭐ **A subreaper shim.** Make the step's entrypoint a tiny static `scarab-shim`
  that sets `PR_SET_CHILD_SUBREAPER`, execs the user command, and records
  `(argv[0], start, end, exit, rusage)` for every descendant it reaps. **Exact**
  boundaries, no privilege, no `shareProcessNamespace`. Cost: it is on the critical
  path of every build — a shim that mishandles a signal or an exit code is
  unacceptable, so it must be perfect or off.
- ⭐ **`shareProcessNamespace: true` + a `/proc`-polling observer container.** That
  field is permitted under PSA `restricted` (it is not `hostPID`). One span per
  direct child of the step's PID 1, depth capped at 1–2 deliberately. **Nothing on
  the user's critical path** — the tradeoff against the shim. Red line: processes
  shorter than the poll interval are invisible and boundaries are sampled, not exact.

⚠ **Security finding (from the observer route, worth recording regardless).**
`shareProcessNamespace` dissolves ADR-0042's credential isolation: a step with
`run-as-root` — a *self-service* grant, no admin involved — can read the results
sidecar's `/proc/<pid>/environ`, where `SCARAB_RESULTS_TOKEN` lives today
(`executor-k8s/src/lib.rs:3007-3012`), and its rootfs via `/proc/<pid>/root`. **Today
this is harmless**: the token is `HMAC(secret, "run:step:attempt")`, so stealing it
grants the step the ability to write *its own* results — which it could already do by
writing the JSON files. But ADR-0042 §5 plans to widen that channel to progress,
annotations and artifact manifests. **So process-namespace sharing is safe now and
becomes a real hole the day the egress channel's scope exceeds the step's own
identity.** Mitigation: keep the egress token attempt-scoped forever, or make the
observer and the egress sidecar mutually exclusive with `run-as-root`. This belongs
in an ADR, not in a postmortem.

## 1.6 Four variations

### O1 — Bridge (1–2 weeks). *Do it regardless.*
`tracing-opentelemetry` as a second layer behind `SCARAB_OTEL_*`; instrument only
spans the process genuinely owns (HTTP → handler → sqlx; one `scarab.tick` span per
run-tick, which ADR-0059 already isolates; one per outbox dispatch, kube call, forge
call, Depot request). Fix the `.instrument()` defect. Add `outbox.trace_context`.
**And build the histograms ADR-0053 promised** — tick duration, admission latency,
log-tail lag — then switch `/metrics` to **OpenMetrics with exemplars** so clicking a
slow bucket lands on that tick's trace. Highest value per line in this document.
Since the `cicd.*` metrics are RC, use their names where they fit rather than inventing
ours: `cicd.pipeline.run.duration` (histogram, seconds), `cicd.pipeline.run.active`
(UpDownCounter), `cicd.pipeline.run.errors`, `cicd.system.errors` (over
`controller|scheduler|agent`). `cicd.worker.count` has no analogue for us — we have no
worker pool — and that absence is itself worth noting in the ADR, because it is the
shape of the architectural difference from every agent-based CI.
**Gets:** operator observability, finally. **Doesn't get:** any Run trace — a Run's
40 minutes appear as ~2000 unrelated 3 ms traces. **Breaks:** nothing. Amend ADR-0053
from "OTel deferred" to "OTel traces adopted for process-local spans; Prometheus pull
retained for metrics."

### O2 — The event log is the trace (4–6 weeks). *The architecturally correct core.*
A projector (`--role trace`, or a converged task) walks `events` by `seq` and emits
OTLP built **after the fact** from `{AttemptStarted, AttemptFinished,
StepTransitioned, RunTransitioned, GateReleased, …}` pairs. Derived IDs; Take = trace;
segment-at-suspend; the drop-too-old counter. Deltas: `trace_export_cursor(exporter,
last_seq, updated_at)` plus a `leases` row reusing the existing lease table — **no
column on `events`, none on `runs`, no stored `take`** (ADR-0056: a Take is a derived
lens). Emit these by hand; do **not** route them through `tracing-opentelemetry`,
whose whole lifecycle model is "a live process owns the span". Two emitters in one
binary is the correct answer, not a smell. **Red lines:** 1 ms resolution forever
(§1.1); the projection can never show what the event log does not record — which
means some phase timings must become *recorded events* first; a backend outage
becomes bounded resumable lag rather than loss.

### O3 — The Attempt as an observability capability (3–5 weeks). *The differentiator.*
§1.5 items 1–3, in that order, opt-in per Step:

```yaml
steps:
  - id: test
    observe:
      otlp: true          # step-local endpoint + TRACEPARENT
      tests: junit        # /scarab/results/junit/*.xml → test.* spans
      processes: sampled  # shareProcessNamespace + /proc observer   (or: shim)
```
**Breaks:** the sidecar gains a second responsibility. Argue it is the *same* one —
"the fenced, trusted egress path for everything a Step says about itself" — or the
sidecar becomes a junk drawer. Also: the sidecar image outgrows `sh + curl + jq`,
which is a good moment to make it a real binary and give it a log tail and a metric,
both of which it lacks today (§1.1).

### O4 — Scarab as an OTel-native observability product (6+ months). *Reject.*
Depot runs an OTLP receiver; logs stored as OTLP LogRecords; ship Tempo+Loki+Grafana;
the Runs UI reads a span store instead of the event log. **It violates ADR-0063 in
precisely the way that ADR names** — a green Attempt whose logs vanished at day 7
under someone else's retention, with Scarab's durable record pointing at data it
neither governs nor can verify — and adds a fourth stateful component after
CONTEXT.md §5 counts three and calls the third deliberate.
**Harvest three things and drop the rest:** (a) `trace_id`/`span_id` columns on
`log_chunks` so the log pane deep-links to the span and back — a two-column delta, no
system-of-record change; (b) a downstream span-metrics connector so fleet questions
are answered by aggregating spans rather than minting Prometheus series; (c) **the UI
waterfall** — we do not need a span store to render *our own* waterfall, because the
event log already contains it, and a good run view is not a tracing backend.

### Should OTel logs replace the ADR-0063 decision? No.
ADR-0063 turns on a property an OTLP pipeline cannot supply: *absence is
authoritative*. Its read path needs "are the bytes there?" answered **against the
storage, on the read**. A fire-and-forget exporter with a collector queue in the
middle cannot distinguish "the step printed nothing", "the collector dropped it", and
"Loki's retention expired it" — exactly the blank pane ADR-0063 exists to prevent.
Scarab should be an OTLP-logs **producer, never a consumer**, and ADR-0063 §8's
"pinned until a durable sink acknowledges" can simply treat an OTLP export ack as a
sink ack. The worthwhile inversion: the step-local endpoint can accept **OTLP logs
from user code** and land them in the same `{run, step, attempt}` chunk namespace as
a second stream (`stream=otlp`) — structured step logs as first-class evidence
without a second system of record.

## 1.7 Recommendation

**O1 now → O2 as an ADR → the active-time trace → O3 in order → the UI half of O4.
Never O4's storage tier. Never eBPF without a named operator asking.**

The doctrine to write into the ADR, generalising ADR-0063:

> The **event log is the system of record**; OTLP is a **sink**. A trace is a
> *projection* of durable state, never the origin of it. Nothing about correctness,
> evidence, or retention may depend on a span being ingested.

## 1.8 Two ideas worth more than the tracing

⭐ **Content-identity-backed flake detection.** ADR-0061 gives every Attempt a
**content identity** for its inputs. So Scarab can state something no other CI can:
*"Attempt 1 failed and Attempt 2 succeeded on byte-identical inputs"* — a
**machine-checkable definition of flaky**, not a heuristic over test names. Combine
with O3's test spans and you can attribute the flake to a named test, with proof. This
is a *product feature*; OTel is merely how it ships.

⭐ **One egress protocol — tempting, and the trap is worth naming.** The results
sidecar and an OTLP receiver are the same shape: a fenced localhost endpoint a Step
writes structured facts to. So publish a **Result** as a span attribute
(`scarab.result.<name>`), logs as OTLP logs, and collapse three contracts into one —
killing the footgun where a bare filename publishes nothing and the step still exits
0. **But** that makes an OTel path load-bearing for correctness, contradicting §1.7.
The defensible middle: the sidecar may *accept* OTLP as an input encoding, then write
a Result through the existing fenced ingest path. OTLP as a wire format we accept —
never as the channel evidence travels on.

---

# Part 2 — Agents

## 2.1 The headline disagreement

Your sketch is "`agent` as a **sibling** of `step`". I think that is wrong, and
[ADR-0058](../adr/0058-runtime-service-containers.md) already supplies the test.
It demoted `service` *out* of being a step kind on three criteria: a DAG node has
**an id, an exit code, and an Attempt other steps can `need`**. A Service has none.

An agent has **all three**. It is addressable, it terminates with a verdict, and
downstream steps genuinely want to `need` its Results. So the ontologically correct
answer is the opposite of ADR-0058's: **`agent` is a Step *kind*** — a sibling of
`clone`/`gate`/`invoke`, an optional field on `StepSpec`, which is already the shape
of that struct. Not a new plane.

What an agent *does* need that no existing step kind needs is a **finer grain inside
the Pod boundary**, and that is §2.2(2).

Two corollaries, immediately:

- **The "monolithic LangGraph server" case needs no new concept at all.** A
  Run-scoped Pod with a cluster DNS name that opt-in Steps reach **is a Shared
  service** (ADR-0058), driven by a thin agent Step. Be honest about the cost
  ADR-0058 already states: a Shared service is *unfenced external mutable state*, so
  if LangGraph holds its own checkpointer, the Transcript lives in *their* Postgres
  and our claim degrades to "we durably re-drive; they own the checkpoint."
- **Agents supply *data* to the DAG, never *code*.** Whatever dynamic behaviour we
  allow, the pipeline being expanded is human-authored and in-repo at the triggering
  ref. The agent chooses coordinates; a human wrote the shape. That is the sentence
  that keeps the whole governance model intact.

## 2.2 The four collisions

**(1) The durability contract.** CONTEXT §2 says interiors are not replayable; §3
says Scarab is not a general durable-execution engine. Options: **bend it** (Temporal
-style deterministic replay — needs a per-language SDK and a determinism sandbox, and
reverses ADR-0002 *and* ADR-0008's zero-SDK choice); **shape the agent to fit** (a
chain of opaque wholesale-re-executed units, continuation in the data plane); or
**move the boundary one level down** (the unit of at-least-once becomes the **Turn**).

Take the third, delivered by the second's machinery. The claim must be written
*exactly* this way or it becomes a lie:

> Scarab does not make an agent's interior durable. It makes the agent's
> **Transcript** durable and re-feeds it.

We never re-execute a prior Turn, so we never require determinism of user code — which
is why this is not Temporal replay and does not reverse ADR-0002. A Turn is an opaque
black box re-executed wholesale, exactly like `terraform apply`. Non-determinism means
a re-executed Turn may take a *different* action; that is the ADR-0021 double-effect
hazard with no new mitigation and no new excuse — the fence is threaded into every
tool call as its idempotency key, and non-cooperating sinks double-fire. Say it the way
ADR-0002 says it.

**(2) The grain of evidence.** Turn ≠ Attempt: Attempts are *alternatives* to each
other (`attempts.consumed`, the retry budget, `Superseded`, latest-successful
of-record resolution all assume this), while Turn 5 is a *successor* to Turn 4.
Conflating them destroys the ADR-0056 read model. Turn ≠ StepRun either, because the
DAG is static and the turn count is unknowable at submit. So: a new grain **between**
StepRun and Attempt, and the fence widens `{run, step, attempt}` →
`{run, step, turn, attempt}` with `turn = 0` for every non-agent step (expand-contract,
ADR-0022). Then everything else falls out cleanly:

- **A Turn is the Pod boundary.** A 6-hour agent is not one 6-hour Pod; it is a Turn
  sequence, each under its own `timeout:`.
- **Retry** = re-execute the current Turn from the last durable Transcript.
- **Rerun at Turn k** = fork a Take and truncate the Transcript to k−1.
- **Superseded** = an in-flight Turn cut short because a human reran an earlier one —
  ADR-0056's definition verbatim, no amendment needed.

**(3) The static DAG.** Four options, and the *elegant* one is not the best one:
*leaf producing Results* (zero delta — covers PR triage, release notes, flaky-test
classification); *callback that launches Runs* (ADR-0025 `on: api`/`upstream`, zero
engine delta, RBAC-gated, causally linked in the event log); *dynamic sub-DAG* (needs
ADR-0023's reserved `expand` event, which is **not built** — I checked: matrix
expansion is submit-time only, `scarab-pipeline/src/lib.rs:827-911`, and no `Expand`
variant exists); *agent-as-orchestrator* (**reject outright** — it subordinates a
statically-validated DAG to untrusted model output and leaves admission nothing to
admit at creation).

⚠ **The ordering trap.** A dynamic sub-DAG inherits the parent's Environment and
secret scope; a spawned Run is admitted, approved and budgeted separately. **For
agents, the Run is the governance boundary and the Step is not.** So the
callback-launches-a-Run answer is not merely cheaper — it is *safer*, and the elegant
option is the worse one.

**(4) The image contract.** A thin protocol over the existing seam — not a server,
not an SDK. One new directory on ADR-0008's convention: the agent reads
`/scarab/agent/state.json` (the Transcript so far) and writes `/scarab/agent/turn.json`
= `{verdict: continue | suspend | done, transcript_delta, results?, wait?}`, which
ADR-0042's sidecar drains with acked delivery over the channel that already carries
`results/*.json`. Any runtime, zero SDK, ADR-0008's ecosystem argument intact.

## 2.3 What Scarab already has that a Hatchet clone must build

This is the argument for doing it at all:

durable suspend with machine release (`gate` + ADR-0034 HMAC) — **human-in-the-loop
and webhook-delivered tool results, done**; typed launch Parameters with a describe
endpoint and generated form (ADR-0043) — **the invocation surface, done**; typed
Results + acked egress (0041/0042) — **guaranteed structured output, done**; scoped
secrets with fork-PR lockout and log redaction (0014/0037) — **model-key governance,
done**; an OIDC issuer — **keyless tool federation, done**; the restricted PSA
baseline + digest-keyed governed Grants (0039) and `runtimeClass` already carried in a
PlacementProfile overlay (0055) — **gVisor for model-generated code is an operator
config away, not a new plane**; per-Pod NetworkPolicy (0042) — **per-tool egress
allowlists are one field away**; Matrix + `all-complete` join — **evaluation runs are
nearly free**; event log + SSE + Takes — **audit, streaming, time travel**; `budget:`
active-time per Take (`scheduler.rs:2494`) — **the sibling slot for tokens**.

The missing 20%: the Turn grain, the Transcript, token/cost budgets, per-tool-call
audit, and a step-level suspend (§2.5).

## 2.4 Five variations

### A1 — Agent as a Library pipeline (days). *No new concept.*
`.scarab/lib/agent.yaml` plus an official `scarab-agent` image; `interface.inputs`
carries `prompt`/`model`/`max_turns`; output is a Result.
```yaml
- id: triage
  invoke: .scarab/lib/agent.yaml
  with: { prompt: "${{ inputs.issue_body }}", model: sonnet, max_turns: "12" }
  needs: [clone]
```
**Deltas:** none to engine or IR. **Risk:** no durable multi-turn — one Pod under one
`timeout:`, so a node death loses six hours. Copy must never say "durable agent".
Worth doing as a one-week spike, dogfooded on this repo's already-live
`.scarab/dogfood.yaml`.

### A2 — `agent:` as a Step kind; **Tools as Sidecar services** (2–4 weeks)
Single Turn from the engine's view; the loop runs inside the Pod, non-durable, like
`cargo test`. The agent is a leaf producing Results. A **Tool** is a declared,
governed capability realised as an MCP Sidecar service on `localhost` — and the agent
container gets **no egress at all** except to its declared tool sidecars, *model
access included*, via a metering `llm` proxy that holds the key.
```yaml
- id: fix
  needs: [clone]
  agent:
    model: sonnet
    prompt_file: .scarab/prompts/fix.md
    max_turns: 30
    tools:
      - name: repo
        image: ghcr.io/scarab/mcp-fs:1        # workspace-scoped, no egress
      - name: jira
        image: ghcr.io/acme/mcp-jira:2
        secrets: [JIRA_TOKEN]                 # ADR-0037 scope chain; agent never sees it
        egress: [jira.acme.com:443]           # NetworkPolicy on the sidecar, not the agent
  budget: { tokens: 2_000_000, usd: 25 }
  outputs: [src/**]
```
**Deltas:** `AgentSpec`/`ToolSpec` in the IR; executor stamps tool sidecars + metering
proxy + deny-all NetworkPolicy on the agent container; `budget:` grows `tokens`/`usd`
enforced per Take beside active-time (exhaustion → `DeadLettered` with diagnostics,
shape unchanged); per-tool-call events on the log (fence, tool, argument hash,
latency, result hash — bodies on the Depot with their own retention class).
**Language:** *Tool* is a new term but slots in as "a Sidecar service with a declared
purpose"; ADR-0058's "not a `needs`-able DAG node" holds. **Risk:** the interior loop
is where the value is, and the first user hits the deadline and asks for suspension
the same week.

### A3 — The Turn grain (6–10 weeks on top of A2). ★ *recommended target*
Three new terms, no new plane. **Turn** — the unit of at-least-once inside an agent
Step and the Pod boundary. **Transcript** — the agent's append-only log-of-record;
since ADR-0063 already established that content addressing buys nothing for an
append-only stream, it lives on the **Data Depot** beside Step logs, offsets in
Postgres, pinned until a durable sink acks, absence authoritative. **Tool** as in A2.

The engine shape: an agent StepRun whose Turn returns `continue` becomes **Ready
again** — a self-loop on a single node. **The node set stays static; only the Turn
sequence grows.** That is how you get unbounded agent length without a mutable DAG.
```yaml
- id: implement
  needs: [clone]
  agent:
    model: sonnet
    turn_timeout: 900              # per-Turn Pod deadline
    max_turns: 200                 # forward-progress bound
    may_request_gates: [plan-ok]   # may ARM a declared gate, never invent or release one
    tools: [...]
  budget: { active: 4h, tokens: 20_000_000, usd: 200 }
- id: plan-ok
  gate: manual
  needs: [implement]
```
**Deltas:** the 4-tuple fence (expand-contract, `turn` defaulting to 0) touching
`attempts`, artifact keys, log keys and `?attempt=`; `TurnCompleted`/`TurnSuspended`
events; Transcript as a Depot class with a RetentionProfile entry; UI evidence axes go
from version × step × try to version × step × **turn** × try — a two-level filmstrip,
a real cost, not a footnote. **Language:** CONTEXT §4.3 gains **Turn**, §4.2 gains
**Transcript**, §2's "at-least-once per step" becomes "per step, or per Turn within an
agent step"; §3's non-goal survives and should be reasserted. **Risk:** the fence
widening touches the crash/resume tests that are the only *proven* part of the wedge —
budget real DST work, not a migration.

### A4 — Bounded dynamic fan-out (8–12 weeks on top of A3). *On demand only.*
The agent emits a Plan Result; a declared subgraph expands over it with a hard cap.
```yaml
- id: work
  invoke: .scarab/lib/unit.yaml
  for_each: ${{ results.plan.units }}
  for_each_max: 50
  needs: [plan]
  join: all-complete
```
Shape static, cardinality dynamic. **Breaks ADR-0023's core invariant by name**, needs
the `Expand` event, a growing node set, DST over a mutable DAG, and a graph that grows
mid-run in the UI. Gate it on a *named* consumer — and remember §2.2(3)'s ordering
trap says the spawn-a-Run alternative is usually better anyway.

### A5 — An Agent plane: Agent as a peer of Pipeline. *Refuse.*
A second top-level durable entity with its own Environment, a Transcript persisting
across Runs, an inbox, launching Runs rather than living inside one. This is the
"one-stop shop" shape. It reverses "a Run is the durable instance" as the top of the
run-time ontology and CONTEXT §3 wholesale, costs a quarter-plus, and puts us against
Temporal on Temporal's axis with no SDK. Its only genuinely new content is
**cross-Run agent memory** — and that is a keyed Cache (ADR-0065) or a retention
class, not a plane. Calling it an architecture is how A5 gets built by accident.

## 2.5 Five engine facts that price A2/A3 honestly

These move the estimate in both directions, and two of them are traps.

**A2 is cheap, structurally.** There is **no `StepKind` enum** — kind is
mutually-exclusive `Option` fields on one `StepSpec`
(`scarab-pipeline/src/lib.rs:279-330`), and the *entire* system switches on kind in
exactly one place: the `if/else` in `persist_run_from_ir`
(`scarab-server/src/lib.rs:4209-4377`), plus a reduced duplicate on the inline-API
path (`:1151-1240`). **The scheduler is kind-blind except for gates.** So `agent:` is
an IR field + one arm + an executor path + a `LocalExecutor` rejection (the pattern is
`executor-local/src/lib.rs:141-169`) + forwarding through both decorators. New
`EventPayload` variants need **no SQL migration** (storage is pure serde, and
`Raw(Value)` at `engine/src/lib.rs:863` is the forward-compat hatch).

**The self-loop is cheaper than it looks.** `Running → Ready` is already a legal step
transition — it is literally what `rearm_step` does after a retryable failure
(`scheduler.rs:2831`), minting a new attempt id and therefore a new fence. A Turn
boundary reuses an existing, tested, zombie-fencing edge.

⚠ **Trap 1 — the retry budget is computed by replaying the event log**, not from a
column: each `AttemptStarted` increments `used`, each `RunRerunRequested` resets it
(`scheduler.rs:2745-2770`), and `allowed` defaults to 1. So **a naive Turn-per-Attempt
implementation dead-letters a 200-Turn agent at Turn 2.** Turn-advancing attempts must
be distinguishable from retry attempts in that replay. This is small code and easy to
miss, and it fails *closed* in the most confusing possible way.

⚠ **Trap 2 — suspend-between-Turns, the real cost centre.** `Suspended` is a
**`RunStatus`**, not a `StepStatus` (`engine/src/lib.rs:68-76`, `141-149`), and the
gate pre-pass suspends the **whole Run and returns immediately**
(`scheduler.rs:1495-1503`) — so today a ready gate is a **global barrier**; every
parallel branch stops. Fine for a gate (ADR-0008 says "suspends the whole run", and a
gate is usually a join). **Not** fine for an agent waiting on a tool result while three
other steps build.

But there is a cheaper way out than a new `StepStatus`, and it is already in the tree:
the **shared-service readiness hold** (`scheduler.rs:1537-1594`) keeps an individual
step at `Pending` via a *tick-time predicate* — a per-step wait with no global
barrier and no new state. Make the predicate read a **durable** fact (the same
two-column trick gates use: `step_runs.gate_kind` + `gate_timer_seconds`,
`migrations/0011`, `0014`) and you get a durable per-step suspend with no new
state-machine edge. That is the design to try first; the new-status route is the
fallback, and it costs DST cases and a UI status.

**And durable liveness does not exist at all.** There is no heartbeat anywhere in
`crates/` — liveness is poll-plus-deadline. The one fence-grained pattern to copy is
the log-tail lease: `lease("tail:{run}:{step}:{attempt}")`, TTL 45 s, renewed at TTL/3
with takeover on holder crash (`log_tail.rs:161,266-285`). A long-lived agent Turn
needs exactly that, built.

**One thing agents need is already built and I did not expect it:** duplex
WebSockets to a running step — `attach_step` and `debug_pod_step`
(`scarab-server/src/lib.rs:3193,3281`) over `StepAttacher`/`DebugLauncher`,
Administer-gated and k8s-only. Interactive steering and token streaming have a
precedent; note both sit *outside* the state machine, which is the right place for
them.

## 2.6 Product compass

**"One-stop shop for async needs" is a losing frame, and it is losing for structural
reasons, not taste.** Zero-SDK (ADR-0008), Kubernetes-only (ADR-0005), and a
deliberately *bounded* state machine (§3) are each an asset in the CI market and a
liability in the generic-async market. A Pod per unit of work cannot compete with
in-process task execution for a queue of 10k small jobs; Hatchet does that and we
structurally cannot. Entering that market means reversing all three decisions —
i.e. becoming a different company with a worse starting position.

**What the market actually looks like** (researched, not assumed):

- **Hatchet** is an API server + engine + **SDK workers over bidirectional gRPC**,
  Postgres as the source of truth for both runtime state and observability,
  at-least-once. Its durable tasks **checkpoint, evict the worker slot, and replay the
  event log on resume — so the code between checkpoints must be deterministic**. That
  is the constraint our Turn model deliberately does not take on, and it is the honest
  comparison sentence: *Hatchet replays your code; we re-execute your Turn.* It is
  **SDK-authored, with no OCI-image task shape**, is "not recommended for sustaining
  10,000+ tasks/sec", and — verified absence — **has no token or cost budgets
  anywhere**. Pricing: $10/1M runs, Team $500/mo, Scale $1,000/mo (audit logs + HIPAA
  land at that tier, which tells you who buys it).
- **The same absence holds across Inngest, Restate, DBOS, Trigger.dev, Cloudflare
  Workflows and Step Functions**: all have durable mid-function suspension and
  human-in-the-loop; **none has a token or cost budget**. Temporal's nearest thing is
  spend-governance "Capacity Modes", and its new **Principal Attribution**
  (non-spoofable "who invoked this") is the same instinct as our unforgeable Actor.
  Only **LangSmith Deployment** has real cost telemetry and per-tool-call tracing —
  and it *measures*, it does not *enforce*.
- **Execution shape is the gap.** Trigger.dev is the only one that deploys tasks as a
  Docker image, and it is still SDK-authored; Step Functions is container-capable with
  no agent semantics. **Nobody combines OCI-image task execution with per-tool-call
  governance.**
- **The coding-agent products have the governance holes we would close.** GitHub's
  Copilot coding agent runs on an ephemeral Actions runner with a default-on egress
  firewall — but **the firewall covers only the Bash tool, not MCP servers or setup
  steps**, and Copilot is added as a **ruleset bypass actor** because it cannot satisfy
  some rulesets. That is precisely the pair of failures A2's design forecloses (a
  NetworkPolicy on the *sidecar* covers every tool including MCP; and nothing in
  Scarab would bypass protection rules). OpenAI's Codex cloud is the closest to our
  shape and validates it: a two-phase runtime where **setup has network and secrets and
  the agent phase runs network-off with secrets removed**.
- **Two datapoints to sit with.** **Cursor shipped GA self-hosted cloud agents on
  customer Kubernetes (2026-03-25)** — worker, tool execution and secrets in your
  cluster, only inference theirs. That is the competitor closest to this seam and it is
  already shipping. And **Terragon, a standalone cloud background-agent orchestrator,
  shut down in January 2026** — the standalone orchestrator is a hard business; the
  governance-attached one may not be, but that is a hypothesis, not a finding.
- **The named gap:** *a governed pipeline with approval gates around agent steps was
  not found as a shipped product anywhere.*

**The defensible seam is the governed coding-agent control plane.**

- **Customer:** the platform/DevEx team at a 200–5000-engineer company that has just
  been told "let engineers point agents at our repos."
- **Job to be done:** *let agents touch our repositories and our infrastructure under
  the controls we already require of CI* — fork-PR lockout, Environment approvals,
  digest-pinned images, OIDC instead of long-lived cloud keys, namespace-per-run
  isolation, an immutable event log, retention.
- **The differentiated sentence: "the agent is a Run."** Everything you already
  trust about a Run applies to it. Nobody else can say that, because nobody else has
  the CI governance model underneath.
- **Adjacent and nearly free: evaluation as CI.** A matrix over models/prompts, an
  `all-complete` join, Results and Artifacts, `on: pull_request` — an eval suite
  gating a prompt change the way tests gate code. This falls out of machinery that
  already exists.

⚠ **This earns a narrow AI claim, and `docs/positioning.md` currently forbids all of
them** ("Never write AI-native CI… there is none in the code"). That rule was right
when written and becomes wrong the day A2 ships. It needs an **explicit amendment**,
not a quiet violation — and the amendment should keep the discipline: claim
*governance of agents*, never *intelligence*.

### What I would refuse to build

- **A per-language durable SDK with checkpoint primitives.** ADR-0008 chose zero-SDK
  to inherit the container ecosystem on day one; an SDK is Temporal's and Hatchet's
  moat and starts our catalogue at zero.
- **Deterministic replay of agent code.** Reverses ADR-0002 and buys a determinism
  sandbox we maintain forever.
- **Agent-authored dynamic DAGs / agent-as-orchestrator.** Untrusted model output
  would decide what admission admits, and nothing would be statically validatable.
- **A generic async job runner or task queue.** §2.6's opening paragraph.
- **A hosted multi-tenant sandbox for adversarial agent code.** CONTEXT §3's
  explicit non-goal; the honest answer is a `runtimeClass` PlacementProfile plus
  telling operators plainly that v1 is untrusted-but-not-adversarial.
- **A model gateway / LLM router, prompt registry, or vector store.** The metering
  proxy is a NetworkPolicy enforcement point and must never grow into a routing
  product.
- **Cross-Run agent memory as a new plane.** If needed, it is a keyed Cache.

## 2.7 Three ideas worth more than the plumbing

⭐ **Rerun-from-Turn is prompt bisection, and it is nearly free.** A Rerun already
forks a Take and re-arms an invalidation set, so "rerun this agent from Turn 7 with
Turns 1–6 replayed verbatim from the Transcript, and one Parameter changed" is
existing machinery plus a truncation offset. Nobody ships a *resumable fork of a
trace inside the governed run that produced it* — LangSmith gives you a read-only
trace.

⭐ **Tools as Sidecar services make the allowlist physical, not advisory.** Deny-all
NetworkPolicy on the agent container; every capability — including the model —
reached through a declared `localhost` MCP sidecar that holds the secret and carries
its own egress allowlist. The agent then *cannot* call an undeclared tool or an
undeclared model, and **the token/USD budget is metered at the proxy rather than
self-reported by the thing being budgeted.** Every competitor's allowlist is a prompt
or an SDK check.

⭐ **The overlayfs upper layer is a kernel-attested change set.** ADR-0062 already
delivers a Workspace as an overlay whose upper layer is "the kernel's own record of
what was touched — exact, and resting on nothing". For an agent that is an
unforgeable per-Turn audit record of exactly what it wrote to disk — the artifact
coding-agent products approximate with `git diff` *inside the sandbox they are
auditing*.

---

# Part 3 — Where the two halves meet

They are the same machinery twice, which is the main argument for sequencing them
together:

1. **The Turn is a span.** Part 1's derived-ID scheme extends to the 4-tuple with no
   new thinking, so an agent's trace is a Turn sequence under a step span — the
   agent-trace product (LangSmith's core) *is* O2 applied to A3.
2. **The Transcript and the Step log are one retention class.** Both are
   append-only, neither is re-derivable, both are pinned until a sink acks
   (ADR-0063). A3 adds a class, not a mechanism.
3. **The metering proxy and the OTLP receiver are the same sidecar.** A2's
   token/USD metering point and O3's step-local telemetry endpoint are one container
   with two listeners — build it once, as a real binary replacing today's shell
   script.
4. **Per-tool-call audit events and O2's projector are one pipeline.** Tool calls
   land in the event log; the projector already turns event-log entries into spans.

**Suggested joint order:** O1 (fix the blind engine, 1–2 wk) → A1 spike (days) → O2
(the projector) → A2 (`agent:` + Tools + budgets — *this is the demo that wins a
platform team*) → O3's sidecar rewrite, which A2 needs anyway → A3 (the Turn grain,
after the step-level `Suspended` work is scoped honestly).

# Open questions for you

1. **Is the governed-coding-agent framing the one you want?** §2.6 argues it is the
   only defensible version of "agents in Scarab", and it implies amending
   `docs/positioning.md`. If your ambition is genuinely the generic async market,
   A5 is the honest path and most of this document argues against it.
2. **`agent` as a Step kind, not a sibling plane** (§2.1) — do you accept the
   ADR-0058 test, or is there a property of your sketch I have not accounted for?
3. **Does the 1 ms trace resolution matter to you?** It is a permanent consequence of
   `events.at` being millis, and the alternative (a `finished_at` on `attempts` in
   micros) is a migration we could do now and never again.
4. **How much do you want to spend before a named user?** O1 and A1/A2 are
   inexpensive and mostly de-risking. O2 and A3 are the real bets — each 4–10 weeks
   and each touching the crash/resume tests that are the wedge's only proof.
5. **The eBPF question is really a DaemonSet question.** ADR-0062 refused a node
   driver for the *data* path. Would you spend it for telemetry? I would not, but it
   is your call and it is the one door §1.5 leaves closed.

---

# Part 4 — The Mandate and the steward (added 2026-08-01, after discussion)

> **Naming update:** "the steward" has since been nicknamed **Orion**; the
> product specification lives in
> [orion-product-spec.md](orion-product-spec.md). Architecture below is
> unchanged — read "steward" as Orion.

Parts 1–3 were written before a framing conversation that settled several of the
open questions above. This part records where that landed. **Status: direction
agreed in discussion; still pre-ADR.** It supersedes Part 2's sequencing where they
disagree (notably: A3's engine surgery is deferred, possibly forever).

## 4.1 The frame: engine public, AI is the multiplexer

The chosen lens is the Superlogical/libghostty move: keep the craft artifact a
public building block, and found the product one layer *up* from it, consuming it
exactly as it was designed to be used. Applied here:

- **`scarab-engine` + `scarab-pipeline` are the libghostty** — pure domain crates,
  zero infra deps, compiler-enforced (CONTEXT invariant 4). Already extractable.
- **The AI layer is the multiplexer.** A terminal multiplexer's value is sessions
  that survive detach, many concurrent, attach-and-steer any one. That is the
  durable Run, the concurrency model, and `attach_step` — Scarab is already a
  multiplexer for work; it calls its sessions Runs. The AI product composes them.
- The owner has said the layer may **be given away free later** — so the seam below
  must stand on architectural merit (coupling discipline, evolution pace), never on
  a monetisation boundary.

Six altitudes where AI can sit, with the compass rule that keeps all of them
honest — **AI may propose; only a Run disposes**:

| # | Altitude | What it is | Verdict |
|---|---|---|---|
| 1 | below | advisor to the engine (flake classification via content identity, failure clustering, cost prediction) | build; never load-bearing |
| 2 | beside | explains Runs over the evidence corpus ("why did this fail") | the daily-habit wedge |
| 3 | inside | the `agent` Step kind (Part 2's A2) | the governance demo |
| 4 | above | **the Mandate layer** (this part) | the company |
| 5 | author | pipeline synthesis / GHA migration | a feature, not a product |
| 6 | interface | conversational ops over the API | sugar |

## 4.2 The Mandate (decided name; was "Intent")

Named for the *authority*, not the goal — the differentiated content is the bounds:
budget, tools, approvals, done-condition. Vocabulary:

| Term | Meaning |
|---|---|
| **Mandate** | The durable object: goal + terms (token/USD/time/turn budgets, agent image, allowed tools, approval rules) + done-condition + ledger. **Finite** ("upgrade to React 19") or **standing** ("triage every red main build" — wake-condition includes repo events, never terminates). |
| **Turn** | One **Run** launched under a Mandate. *Simpler than Part 2's A3*: a Turn is a whole Run, not a sub-Attempt grain — no 4-tuple fence, no step-level suspend. |
| **Transcript** | The append-only record re-fed to each Turn. Stored as evidence in Scarab (each Turn's delta = an **Artifact** of that Run), not in the steward. |
| **Verdict** | The Turn's structured proposal in its Results: `continue \| wait(reason) \| done`. **A proposal, never a disposition** — only the done-condition, evaluated against external evidence (CI green, PR merged), ends a Mandate. Never let the thing being governed report its own success. |

What a Mandate *does* is deliberately almost nothing — durable bookkeeping around
an opaque loop, exactly what the engine is for a DAG: (1) holds the contract;
(2) launches Turns via the existing `on: api` path with transcript-so-far as
input; (3) consumes Verdicts; (4) parks holding no Pod — "suspended agent" = no
Run in flight, free by construction; (5) enforces the **cumulative** budget across
Turns (each Turn-Run has its own; the Mandate refuses to launch turn N+1 past the
line — the one thing no single Run can do); (6) answers to a human: pause, kill,
approve, **steer** (inject an instruction into the next Turn's input), **fork from
turn k** (truncate the Transcript, change something, re-drive — prompt bisection
on the existing Take instincts).

**Turn-as-Run resolves Part 2's two traps without engine surgery.** Durability
migrated up a level: a 6-hour agent need not survive node eviction, because the
Mandate re-drives a fresh Run from the Transcript. Every turn is separately
admitted, budgeted, and audited — the safer side of §2.2(3)'s ordering trap, by
construction. The costs are per-turn Pod/admission latency (fine for agent-paced
work) and noisy Run lists — and "group Runs by Mandate" *is* the product UI, so
the cost and the product are the same artifact. ADR-0061/0062 are what make
per-turn workspace re-materialisation affordable. A3's Turn-in-Step surgery
becomes an optimisation for high-frequency turns, taken only if demanded.

## 4.3 The steward (decided name): a separate module, and what "separate" means

**`scarab-steward`** — a steward manages affairs under delegated authority; plain
in the way Depot and Farm are plain. Five seam rules, each doing work:

1. **Separate binary, separate crate family.** Not a `--role` on `scarab-server`:
   the seam is a process boundary, not a CLI flag.
2. **It speaks only the public API** (REST + SSE), authenticated as a service
   principal (ADR-0049), RBAC-scoped per Mandate. Invariant 5 weaponised: if the
   steward can be built on the public API, the engine is *provably* the building
   block. It will immediately hit the verified gap that `GET /v1/runs/{id}/events`
   replays-and-closes (no live tail, `lib.rs:1727-1746`) — building the steward
   forces that fix, which the open product also wants.
3. **Own storage, no shared tables.** Own schema and migrations for mandates,
   ledgers, cursors. Grep-test: `mandate` appearing in `scarab-engine` or
   `scarab-server` is a bug.
4. **Evidence stays in Scarab; the steward stays thin.** Transcript delta =
   Artifact, verdict = Result, changeset = workspace outputs — all existing
   machinery, existing retention. If the steward's database burned down you lose
   the loop's memory and cumulative budgets, never the evidence.
5. **Provenance rides the rails that exist.** A Turn-Run's Actor is the mandate's
   service principal *on behalf of* the creating human; its Headline (ADR-0057) is
   "Turn 7 — upgrade to React 19". The engine stays Mandate-ignorant.

## 4.4 UI harmony: split by ownership, compose by kit

The tangling worry is real but it is **read-side** tangling: the steward UI must
*display* Runs (DAG, logs, gates, takes) that the web-ui already renders — it never
*owns* them. So don't split by page; split by **who owns the object vs who
composes views of it**, and share the view layer as a kit:

- **`ui/kit` (extract):** the embeddable **run surface** — `RunCard`, `DagView`,
  `LogPane`, `GateApprovePane`, `TakeFilmstrip` — fed by an API client + props, no
  routing, no global state. Both apps compose it. Precedent already in-tree:
  `ui/` is a container of two apps (web-ui + docs-ui) sharing `ui/brand`.
  Because the web-ui eats the public API (invariant 5), these components are by
  construction buildable from public data — the kit is the libghostty move applied
  a second time, one level up. It is also potentially the most broadly reusable
  open artifact of the lot: an embeddable Run view (a "checks widget") for
  anyone's dashboard.
- **`ui/steward-ui` (third app):** panes of Mandates composing run surfaces — a
  multiplexer composing terminals, literally. The docket view.

Four harmony rules:

1. **Reads compose in the browser.** No server-side joins, no BFF merging, no
   shared DB. The only cross-system join is: the steward owns mandate → run-ids;
   the run carries an opaque back-pointer (see rule 3).
2. **Writes to engine objects go direct, with the human's own token.** Approving a
   gate or rerunning a step from the steward UI calls `scarab-server` from the
   browser as *the user* — never proxied through the steward — so the audit trail
   stays honest. The steward's API accepts writes only to steward objects
   (create / pause / steer / fork a Mandate).
3. **Links, not embeds, for full context** — deep-link both directions. Forward:
   mandate pane → run detail URL. Backward: a generic, optional **`context_url`**
   on `api`-dispatch provenance (a small ADR-0057 extension; an opaque URL supplied
   by the API caller) lets the run detail page render "Turn 7 of *upgrade to React
   19* ↗" without the engine knowing what a Mandate is.
4. **One identity, one ingress.** Same OIDC session across both UIs; path-routed
   under one origin (`/mandates` → steward) so cookies and CORS stay trivial.
   Steward-ui gets the same mock mode treatment as `just ui-mock`.

Because none of this leans on a license boundary, "give it away free later" moves
nothing: the seam earns its keep as coupling discipline (the steward iterates at
product-experiment pace; the engine UI at infra pace), and if the steward goes
free, the kit and the API contract are simply more public surface that was built
honest from day one.

## 4.5 What this answers from the open questions

Q1: answered — governed-agent framing, with the multiplexer (Mandate layer) as the
product above it; `positioning.md` still needs its explicit amendment before any
copy ships. Q2: answered — `agent` as a Step kind stands for altitude 3; altitude
4 composes Runs and needed no new plane in the engine at all. Q4: answered in
shape — the steward is deliberately buildable without touching the engine's
crash/resume proofs. Q3 (1 ms resolution) and Q5 (eBPF/DaemonSet) remain open.
