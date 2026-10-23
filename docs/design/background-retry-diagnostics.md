# Background retry diagnostics

A laptop waking or changing networks can interrupt the metadata writes that
claim conversation readers. Independent receiver threads previously printed
the entire backend error on every attempt. With several peers, one interruption
scrolled the prompt repeatedly even though the receiver was still retrying.

`telemetry::RetryDiagnostics` owns the reporting policy. Callers retain their
existing retry policy and error results. One service shares a reporter across
its workers; the reporter has no connection, receiver, task, timer thread or
authority to acknowledge work.

The first failure prints one short notice with the local diagnostic log path.
Ongoing failures produce at most one reminder per minute, across all scopes.
The first error, changed errors and repeat counts go to the existing rotated
local `scode.log` / `sudoclaw.log` sink (`SCODE_LOG_PATH` still overrides it).
These are local diagnostic events, not model messages or remote telemetry.
An unavailable log falls back to a rate-limited terminal notice retaining the
original cause.

A scope identifies an independently failing operation. A healthy conversation
does not clear a different conversation's failure. Recovery is reported only
after all observed failures clear and a healthy check follows a short quiet
period. The message describes receive checks recovering; it does not prove a
peer is online, a send was delivered or every message was acknowledged. Abort,
standby and process exit are not successful delivery.

## Repository audit

The audit inspected tracked Rust, TypeScript/JavaScript and shell sources,
then followed error output and retry/reconnect/backoff matches into their
callers. Generated artifacts, vendored dependencies and test-only output were
excluded from production candidates. The TypeScript ACP extraction was still
an independent draft; it is not a second implementation in this source tree.

| Area | Decision | Reason |
|---|---|---|
| `runtime/src/mailbox.rs`: receiver seat, conversation discovery, reader claim, poll and commit | Share one reporter per receiver | Independent workers retry in the background and previously printed every error. Original waits, locks, leases, cursor writes and delivery decisions are retained. |
| `engine-core/src/subagent_completion.rs` and `subagent_completion/control.rs`: terminal persistence | Share one reporter per completion state | The observation and delivery paths report the same retained terminal events. Scope is the real tool-call ID; reporting does not change qualification, persistence, retries or receipts. |
| `api/src/http_transport.rs`, request scope and renderer retry events | Keep existing mechanism | Finite provider retries already have their own budget, lifecycle logs and UI notifier. Adding another reporter would duplicate progress and could obscure request failure. |
| `runtime/src/mcp_server_manager.rs`, MCP transports/lifecycle | Keep existing mechanism | Initialization limits, connection state, typed refusal and explicit reconnect already belong to the manager. There is no matching unbounded per-tick stderr loop to replace. |
| `nexus-vfs-client/src/lib.rs` | Keep error propagation | The transport returns errors to its caller and repairs its channel; the receiver is the reporting owner. Logging again here would duplicate the same incident. |
| `runtime/src/bash.rs`, tools, todo/config persistence, startup and ACP reply-send errors | Keep explicit errors | These report an individual operation or an uncertain result, rather than an owned background retry loop. In particular an uncertain send must never acquire automatic retry. |
| Telemetry sink write/rotation failures | Reuse a fallible writer | The regular sink keeps its fallback. The retry reporter calls the same serializer/rotation path through `try_record`, and owns a bounded terminal fallback if opening or writing the log fails; it does not recursively report a log failure through that log. |
| CLI commands, examples, build/install and test harness scripts | Keep command output | Their failure output is the requested command result, not unsolicited background noise. |

## Terminal ownership

A bounded notice still must not write over live input. The REPL's existing
stderr capture drains through its inline renderer and streaming UTF-8 decoder.
Unix keeps its nonblocking-pipe implementation. Windows now captures the real
Win32 stderr handle through the already-locked `filedescriptor` dependency;
one pipe reader feeds a bounded queue, and the UI tick drains without waiting
for IO. No workspace `unsafe` exception or engine/renderer channel is added.
Terminal handoff restores the original stderr target and resume reinstalls
capture. Teardown closes its queue and pipe without waiting for a library's
retained stderr clone. Headless/ACP output ownership is unchanged.

## Validation

The receiver process fixture uses the existing fault-boundary backend with
real host locks, conversation discovery, two receiver tails, register writes
and natural commits. Only writes to two exact reader registers fail. It asserts
multiple actual retries, one aggregated notice, no early recovery when one peer
is still failing, detailed errors/repeat counts in the log and delivery of both
original messages after the fault clears. This reaches the reader-claim error
branch; it is not a claim of a real TLS outage or a lease-renewal test.
Additional process cases refuse the log open and, on Linux, fail the actual
write through `/dev/full`, guarding against a second stderr spam loop.
A Linux process case holds the real register fault beyond one production
minute and reconciles the backend's exact failed-write count with the initial,
reminder and recovery records. The reminder's own logged attempt is not a
suppressed repeat. No production interval is shortened for this test.

The offline CLI PTY workflow additionally checks that the original node error
is retained in the diagnostic log while a real model turn, input and original
session resume remain usable. The existing receiver/cursor/completion tests
remain applicable. After the original startup and completed-turn readiness
assertions, the PTY keeps that process alive until its actual background IO
failure appears in the log; a fast Windows mock turn can otherwise exit before
the connection attempt reports a result. The original input/turn deadlines are
unchanged. This caught a real Windows canvas defect when a late stderr
notice overwrote the ready input line; the input assertion remains intact.
The existing long UTF-8 diagnostic PTY case now runs on Windows too and also
requires a clean input line afterward. Exact-head CI and platform results are reported in the PR;
source preparation alone does not constitute runtime rollout or acceptance.
