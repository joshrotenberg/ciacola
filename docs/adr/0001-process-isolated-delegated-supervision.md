# ADR 0001: Process-isolated delegated supervision

- Status: Proposed; implementation remains disabled
- Date: 2026-08-09
- Issue: [#81](https://github.com/joshrotenberg/ciacola/issues/81)

## Context

Ciacola has two authenticated HTTP MCP surfaces today:

- `/mcp` identifies an active agent through its scoped `x-ciacola-agent`
  bearer and installs `Surface::Agent` tools.
- `/mcp-operator` authenticates a human root bearer and installs
  `Surface::Operator` tools. Stdio is also an interactive human operator
  surface.

Those identities solve request attribution inside a cooperative, single-user
process tree. They do not prove which provider process made a request. Claude
and Codex workers can run as the same OS user, and an ordinary worker can have
Read and Bash access. On systems that permit same-UID process inspection, a
prompt-injected worker may inspect sibling process metadata, readable files,
open descriptors, command lines, and ordinary environments. It may also make
loopback network requests.

A bearer copied from a manager process would therefore give an ordinary worker
the manager's authority. Unix peer credentials report the same UID for both
processes and do not distinguish them. A private socket path, parent PID, or
inherited descriptor does not change that fact.

Provider-backed `surface = "operator"` roles consequently remain refused. The
human root bearer must never become the credential for an agent manager. We do
want a manager to publish and wind up work that it dispatched, but only after
the request has provenance that a sibling process cannot replay.

## Decision

Delegated supervision will be a third, brokered MCP principal with its own
router, policy, lifecycle, and audit trail. It is not `Surface::Operator`, does
not install the operator tool catalog, and does not authenticate with either an
HTTP bearer or `x-ciacola-agent`.

The principal is derived from an OS-attested launch instance:

```text
human configuration
        |
        v
isolation launcher/helper ---- broker-owned principal and grant epoch
        |                                  |
        v                                  v
supervisor provider process == attested channel ==> typed repo-worker actions

ordinary provider sibling ---- no attestation ----X
```

The broker must hold or invoke privileged operations on the provider's behalf.
The provider receives no reusable privileged secret. An endpoint name or
rendezvous descriptor may tell the provider where to connect, but possession
of either is not authorization. The broker authorizes a connection only after
the configured isolation backend attests the live launch instance.

### Security invariants

An implementation must preserve all of these invariants:

1. **Distinct principal.** Delegated calls never pass through
   `Surface::Operator`, the human root bearer, or missing-agent fallback logic.
2. **Attested provenance.** The broker derives manager identity from an
   OS-enforced process-isolation identity that an ordinary sibling cannot join,
   inspect, or replay. Caller-supplied headers, agent IDs, PIDs, and parentage
   are not evidence.
3. **Attenuation.** A manager can invoke only the explicit action variants in
   its grant, only for repo-worker assignments descended from that manager.
   Unrecognized actions and fields fail closed.
4. **Inbound control.** An ordinary agent cannot enqueue or resend a supervisor
   turn. Only an authenticated human or an explicitly configuration-approved,
   server-derived source may initiate one. The check happens before a turn row
   or provider work exists. A manager pulls child results and treats their
   contents as untrusted data, never as an instruction that executes by itself.
5. **Non-delegation.** Spawning a role or child agent does not copy broker
   authority. A supervisor grant can be created or widened only through a
   human configuration path, and a revocation path can only narrow or remove
   it.
6. **Non-extractability.** The human root bearer and raw broker or external
   service authority never enter SQLite or another database, `/tmp` or another
   shared temporary path, argv, an ordinary provider environment, provider MCP
   configuration, logs, or audit payloads.
7. **Lifecycle binding.** Authority is bound to one manager agent, one grant
   version, and one attested launch epoch. Restart, retirement, revocation, and
   replacement cannot silently preserve an old channel.
8. **Fail closed.** A missing, unhealthy, unverifiable, or unsupported
   isolation backend leaves delegated roles unavailable. It never falls back
   to `/mcp`, `/mcp-operator`, stdio semantics, or a bearer.

## Threat model

### Protected assets

The boundary protects:

- the human root bearer and all human-only operator actions;
- broker-held authority to publish or clean up repository work;
- the binding between a manager and its descendant assignments;
- the supervisor's inbound decision channel, so an ordinary agent cannot turn
  it into a confused deputy;
- revocation and retirement state;
- the action audit trail and its external-effect reconciliation data.

The ordinary per-agent bearer is still useful for attribution and recursive
agent tools. It is not promoted into a proof of supervisor provenance.

### Adversary

The primary adversary is a prompt-injected ordinary Claude or Codex worker. It
has the role's normal Read and Bash capabilities, can inspect all files and
process metadata available to its OS identity, and can connect to loopback or
discover socket paths. Where the host normally permits it, assume it can:

- list processes and inspect command lines, environments, and open descriptors;
- read same-UID files, including SQLite and temporary files;
- copy any bearer, path, numeric descriptor, or process identifier it finds;
- connect directly to MCP endpoints and replay captured protocol messages;
- claim another agent ID, omit `x-ciacola-agent`, forge ancestry, race restart,
  and substitute another assignment ID;
- call ordinary agent-surface `send` or `resend` against a supervisor, and put
  action-like instructions in a descendant's result;
- spawn shell children inside its own sandbox.

The adversary must not be able to steal, replay, substitute, or mint a manager
principal. Reading non-secret grant IDs or audit rows must not confer authority.

A prompt-injected supervisor is a separate case. It is allowed to exercise its
small grant for its own descendants, because that is the authority deliberately
delegated to the supervisor process. It still must not reach other assignments,
human-only operations, raw credentials, or a way to widen or transfer its grant.

### Trusted computing base and non-goals

The trusted computing base is the OS kernel, the selected isolation backend,
the small launcher/helper and broker, Ciacola's descendant authorization, and
the repo-worker's existing publication and cleanup fences. The human operator
and broker credential source are trusted.

General hostile multi-tenant hosting is out of scope. So are kernel compromise,
host administrator compromise, and arbitrary code execution inside the broker
service identity. Provider API-key and clean child-environment handling are a
separate concern. This decision does not attempt to stop a valid supervisor
from opening a bad draft PR for its own work; review and existing repo-worker
fences remain necessary.

## Isolation backend contract

Delegation is enabled only when a backend can prove all of the following:

1. It launches the supervisor inside an OS-enforced identity or container
   boundary controlled by the launcher, not by provider-supplied arguments.
2. Ordinary workers and other supervisors cannot inspect that instance's
   memory, descriptors, IPC handles, or protected service files.
3. The broker can authenticate a connection as one exact live launch instance
   and bind it to `{manager_agent_id, grant_id, grant_version, launch_epoch}`.
4. The identity is unique enough that another active supervisor cannot
   impersonate it. A UID shared by all managers is insufficient unless another
   kernel-enforced boundary distinguishes individual instances.
5. Teardown closes the channel and prevents the old instance from
   reconnecting. PID reuse and stale socket connections cannot revive it.
6. Backend support and required enforcement can be checked before a manager is
   launched. A partial or degraded setup is reported as unavailable.

The recommended first implementation is a small privileged launcher/helper
that runs the Ciacola service, ordinary providers, and each delegated
supervisor under mutually isolated OS identities. A distinct UID per live
supervisor, or a container/sandbox identity with equivalent kernel-backed
attestation, gives the broker a meaningful peer identity. The helper should
own the instance-to-principal mapping, minimize its protocol, drop privileges
where possible, and keep broker credentials in a service-owned credential
store or memory loaded from a protected descriptor.

Kernel peer credentials are useful only after that separation exists. For
example, a broker may use a Unix-domain peer UID as one attestation input when
the helper assigned a distinct UID to that exact instance. Seeing the same UID
as every worker is not attestation.

Platforms or provider modes without such a backend remain supported for
ordinary agents, but delegated supervisor configuration is rejected before a
provider is started. There is no compatibility mode.

## Broker protocol and principal

The broker exposes a dedicated MCP endpoint containing only the v1 delegated
tools. Its transport address is not secret. The broker derives a
`DelegatedSupervisorPrincipal` from backend attestation before MCP
initialization and attaches it to request context. The provider cannot submit
that principal as JSON or as an HTTP header.

The conceptual identity is:

```text
DelegatedSupervisorPrincipal {
    manager_agent_id,
    grant_id,
    grant_version,
    launch_epoch,
    backend_instance_id,
}
```

These identifiers are non-secret metadata. They become authoritative only
when the broker supplies them after live attestation. The grant record may be
durable, but it contains policy and lifecycle state, not a reusable bearer.

The broker calls the repo-worker's typed domain operations after authorization.
It must not internally synthesize a root bearer, call the generic operator MCP
router, or install every plugin tool and filter by name after dispatch. The
allowlist exists in the broker's request enum and in the repo-worker entrypoint
it calls.

The current core policy vocabulary is intentionally closed and normalized:

- surface: `IsolatedBroker`;
- scope: `DescendantAssignments`;
- inheritance: `Never`;
- actions: `repo-worker/open_pr` and `repo-worker/finish_issue`.

`Never` applies when Ciacola creates another agent or role principal. Internal
provider subprocesses remain confined to the same attested supervisor sandbox,
but receive no transferable grant or credential.

A future backend must define a versioned attestation record that binds the
manager agent, grant ID and version, launch epoch, backend instance, and
complete typed policy. The exact encoding, digest, and key lifecycle belong to
that backend design and are not chosen by this ADR. A provider-supplied display
string can never stand in for the bound identity.

Configuration reserves the corresponding closed shape:

```toml
[delegation.roles."repo-manager"]
actions = ["repo-worker/open_pr", "repo-worker/finish_issue"]
scope = "descendant_assignments"
```

Until an attested backend exists, every nonempty delegation policy has
`Unavailable` status and fails startup validation. Merely accepting this TOML
shape must not activate delegation.

The current reserved shape does not approve automatic inbound sources. Before
any backend can become available, configuration must gain a closed, typed list
of such sources; omission means authenticated-human initiation only. Free-form
source labels and existing access to an ordinary submission API never satisfy
that policy.

## V1 action boundary

V1 has exactly two action variants, named `repo-worker/open_pr` and
`repo-worker/finish_issue`:

A role grant contains an exact, nonempty subset of these variants. The example
policy above deliberately grants both.

```text
OpenPr {
    assignment_id,
    expected_head,
    title,
    body,
}

FinishIssue {
    assignment_id,
    disposition: Retain | RemoveIfMergedOrUnchanged,
}
```

The delegated `repo-worker/open_pr` variant:

- requires a full, exact `expected_head`; the compatibility behavior that pins
  an omitted head is unavailable;
- opens or reconciles a draft PR only;
- derives repository, issue, base, branch, and owning agent from the durable
  assignment rather than caller input;
- preserves the repo-worker's clean-worktree, exact-commit, remote-fence,
  conventional-title, issue-body, and idempotent reconciliation checks.

The delegated `repo-worker/finish_issue` variant:

- identifies only a durable assignment, not an arbitrary agent ID;
- may retain the worktree or remove it only when the existing repo-worker proof
  shows the work is merged or unchanged;
- cannot provide `discard_head` or authorize destruction of unpublished or
  unmerged work.

Before either action, authorization must prove:

1. the attested manager principal and grant are active and not revoked;
2. the manager agent is active and still eligible for the configured
   supervisor role;
3. the assignment was reserved by that manager and every current or replacement
   owning agent resolves through that manager's durable `spawned_by` chain;
4. the lineage has no missing link, cycle, claimed parent, or ambiguous legacy
   ownership;
5. the assignment and action are in a state accepted by the repo-worker's
   existing lifecycle fences.

An assignment is a descendant only through server-derived, durable lineage.
Names, worktree paths, repository names, request headers, and caller-supplied
agent IDs do not establish descent. Missing or ambiguous legacy lineage is a
denial that needs human resolution.

Everything else remains human-only, including:

- `kill`, `prune`, findings adjudication, schedule mutation, admission or
  protection overrides, and arbitrary agent retirement;
- ready-for-review publication, merge, close, comment, arbitrary repository
  selection, or arbitrary Git operations;
- discarding unpublished work through `finish_issue`;
- creating, widening, transferring, or revoking supervisor grants;
- spawning a provider role with inherited native tools or operator authority.

Adding another delegated action requires a new ADR or an explicit amendment to
this one, its own typed schema and scope predicate, adversarial tests, and human
review. A plugin becoming available on `Surface::Operator` never makes it
available to a supervisor.

## Role and descendant behavior

A supervisor role is an eligibility declaration, not authority on its own. The
launcher grants a broker principal only when a human-enabled role, supported
backend, active manager row, and successful attested launch all agree.

An authenticated manager may continue to use ordinary agent-surface tools such
as `start_issue`, `send`, `wait`, and `get`. `start_issue` derives the
implementer's parent from the authenticated manager and records the assignment
owner. The broker independently checks that durable relationship before a
delegated action.

That ordinary surface is outbound from the manager, not an inbound control
channel to it. Agent-surface `send` and `resend` must refuse a supervisor target
for every provider caller, including a descendant, sibling, or the supervisor
itself. The refusal occurs at the submission convergence point before ledger
enqueue, provider launch, session creation, or spend. Omitting
`x-ciacola-agent`, claiming a human source, or calling the same submission path
through a plugin cannot change the result.

Supervisor turns may originate only from authenticated human stdio, the
human-root-bearer HTTP surface, or a typed automatic source explicitly approved
for that supervisor in human-owned configuration. An automatic source is
server-derived and durably recorded; a caller-supplied source string is not
proof. No schedule, webhook, plugin, or recovery path becomes approved merely
because it can already submit an ordinary turn.

Managers collect child work with `wait`, `get`, and assignment resources. A
child response is untrusted input even when it contains an exact action name,
commit, PR body, or imperative prose. Reading it never dispatches a broker
action. The attested manager must make a fresh typed call, and the broker must
independently re-resolve the assignment, lineage, and repo-worker fences.

Children and role-spawned agents receive only their normal agent MCP
configuration. They do not inherit the broker endpoint as authority, a grant,
or the manager's launch identity. Requesting a supervisor role from a provider
child is refused. Provider shell children inside the supervisor's own enforced
sandbox are part of the already-delegated principal; their reach is still
limited to the two typed actions and descendant predicate.

## Restart, revocation, and retirement

Broker sessions are ephemeral even when the manager conversation is durable.
On a Ciacola, broker, helper, or provider restart:

- every live channel closes;
- the previous launch epoch becomes invalid;
- a resumed provider session receives no old bearer or reconnect secret;
- the helper must launch and attest a new isolated instance before the broker
  creates a new epoch;
- persisted policy metadata may be reused only after current role, manager,
  backend, and revocation state are revalidated.

Every queued supervisor turn must carry a durable, typed inbound source. On
startup and role activation, a turn with an ordinary-agent, missing, unknown,
legacy, or no-longer-approved source is refused or settled without provider
execution. Recovery may resume only a turn whose recorded source still passes
the current supervisor inbound policy.

Accepted actions use a durable, unique action ID. The broker records intent and
authorization before the first external side effect, then records completion or
denial. After a crash, recovery reconciles the repo-worker's pinned commit,
publication state, PR state, cleanup fence, and action ID. It never blindly
replays an action because a provider resent a turn.

Revocation increments the grant version and tears down the attested instance.
Manager retirement, removal of supervisor eligibility, explicit human
revocation, or loss of backend enforcement has the same effect. New and queued
actions fail immediately. An action that has already crossed an irreversible
external boundary is reconciled and audited rather than falsely reported as
rolled back; revocation prevents the next effect, not an impossible undo of an
already-created PR.

Retiring or replacing an implementer does not transfer its assignment to a
different manager. Replacement ownership must retain an unambiguous descendant
chain, or delegated action is refused. Retiring the manager revokes the whole
grant regardless of descendant state.

## Audit requirements

The broker records successful and denied requests. Each record contains:

- action ID, action type, time, and policy schema version;
- manager agent ID, grant ID and version, launch epoch, backend name, and a
  non-secret backend instance reference;
- assignment ID, resolved repository and issue, descendant-chain decision, and
  the exact commit for publication;
- normalized policy inputs, with a hash in place of free-form body text when
  the full value is unnecessary;
- authorization result and denial reason;
- repo-worker lifecycle state before execution;
- terminal result, reconciled PR URL/state or cleanup state, and any external
  effect known to have occurred.

The submission audit separately records every accepted and denied attempt to
prompt a supervisor: source type, authenticated human surface and session or
configured source name, target manager, turn identity when accepted, policy
version, and denial reason. It does not copy a raw prompt into a privileged
audit record.

Intent is durable before an external effect and outcome is durable after it.
Restart recovery appends a reconciliation result to the same action ID. Audit
records contain no root bearer, provider credential, broker secret, raw
credential-store handle, connected descriptor, environment dump, or MCP auth
material.

The protected service identity must own the writable audit store. A provider
that can alter audit or grant state has crossed the isolation boundary and the
backend is not eligible. Exporting audit records for review is allowed; reading
them must not confer authority.

## Rejected alternatives

### Reuse `Surface::Operator`

Rejected. That surface is the human root and installs tools whose blast radius
is intentionally broader than delegation. Filtering after installation is too
late and future operator tools would widen the manager accidentally.

### Put the human root bearer or an attenuated bearer in the provider

Rejected. A same-UID sibling can copy a long-lived or short-lived bearer and
replay it during its validity window. Expiry narrows time, not provenance.

### Trust `x-ciacola-agent` or a new identity header

Rejected. Headers are caller-controlled. The existing agent bearer attributes
a cooperative caller but is deliberately not a cross-process isolation proof.
Omitting that header must continue to reject `/mcp`; it can never select human
or delegated semantics.

### Same-UID peer credentials

Rejected. They prove only that both peers use the same account, which is the
condition the threat model assumes. Peer credentials become useful only with a
distinct, launcher-controlled OS identity.

### Process ancestry, PID checks, or executable names

Rejected. They are observable, subject to PID reuse and races, and do not stop
a sibling with same-UID inspection or debugging access. Claimed ancestry is
also just input.

### A private or mode-0600 socket path

Rejected as the authority boundary. Same-UID siblings may discover and connect
to it. Path secrecy and filesystem permissions remain defense in depth after
attestation, not provenance.

### An inherited or already-connected descriptor

Rejected as the authority boundary. Same-UID process inspection can expose or
duplicate descriptors on supported hosts, and a copied connection is
replayable. A descriptor may be a rendezvous mechanism only when the isolation
backend independently prevents access and the broker attests the peer.

### Provider CLI sandbox settings alone

Rejected unless a backend can attest and enforce them for the entire provider
and all of its children. A requested sandbox mode or successful launch flag is
not proof that siblings are isolated from process memory, IPC, or service data.

### Let descendants push results into the supervisor

Rejected. An ordinary worker that can enqueue a manager turn can ask the
manager to exercise authority it cannot exercise itself, making the manager a
confused deputy. Descendant work is pulled and treated as untrusted data.
Ordinary agent-surface `send` and `resend` never target a supervisor.

## Implementation and release gates

The supervisor role remains absent or rejected until all gates pass for every
advertised platform and provider mode.

### Contract tests

- The delegated router contains exactly `repo-worker/open_pr` and
  `repo-worker/finish_issue`; an operator plugin added later does not appear.
- Unknown actions and fields fail before domain side effects.
- `open_pr` requires an exact head and produces only a draft.
- `finish_issue` refuses `discard_head`, arbitrary agent IDs, dirty removal,
  and unproven unpublished removal.
- Missing, cyclic, cross-manager, claimed, and ambiguous legacy ancestry is
  refused. Direct and valid indirect descendants succeed.
- A child role never inherits a grant, channel, or broker configuration.
- Authenticated agent-surface `send` and `resend` to a supervisor refuse before
  a turn row, executor submission, provider session, or provider work. Cover a
  sibling, direct descendant, indirect descendant, self-send, forged target,
  omitted identity, and direct invocation of the underlying submission path.
- Human stdio and root-bearer HTTP submission succeed, while only exact typed
  automatic sources configured for that supervisor succeed. Caller-claimed,
  unknown, and ordinary plugin sources fail before enqueue.
- A child result containing a valid-looking delegated action is inert until the
  manager pulls it and makes a separately authorized typed broker call.

### Adversarial isolation tests

Run a real ordinary sibling with Read, Bash, loopback network access, and all
same-UID inspection normally available on the host. Give it the broker address,
manager PID, non-secret grant metadata, captured MCP messages, and any numeric
descriptor or socket path it can discover. It must fail to:

- inspect manager or broker secrets and descriptors;
- connect as the manager or reuse a manager connection;
- replay a prior request or launch epoch;
- substitute a sibling assignment or forge descendant lineage;
- mint a principal by claiming headers, PID, UID, role, or agent ID;
- make a request without `x-ciacola-agent` look human or delegated.
- prompt or re-prompt the manager through `/mcp`, including with a copied child
  result that asks the manager to publish or clean up work.

The same harness must demonstrate that the attested manager succeeds. A test
that merely checks the sibling was not handed a token is insufficient.

### Lifecycle tests

- Restart Ciacola, the broker, helper, and each provider at every boundary:
  before intent, after intent, after publication, and during cleanup. Old
  epochs stay dead and each action reconciles exactly once.
- Revoke before dispatch, while queued, and immediately before an external
  effect. New effects stop, and any already-crossed effect is reported
  honestly.
- Retire the manager and verify all channels fail. Retire or replace an
  implementer and verify scope neither transfers nor becomes ambiguous.
- Change or remove the supervisor role and verify restart does not restore
  broader authority.
- Restart with queued supervisor turns from human, approved automatic,
  ordinary-agent, missing, legacy, and revoked sources. Only sources still
  approved may reach provider work.
- Corrupt or remove backend attestation and verify startup and reconnect fail
  closed without launching a privileged provider.

### Audit tests

- Success, denial, replay, revocation, restart reconciliation, and external
  partial completion all produce one traceable action history.
- Accepted and denied supervisor-turn submissions record typed inbound source
  provenance without storing raw prompt or credential material.
- The audit identifies principal, grant version, launch epoch, descendant
  decision, assignment, exact commit, and external result.
- Byte scans of SQLite, WAL/SHM files, temporary paths, argv captures,
  environment captures, MCP profiles, and logs find no raw privileged
  capability material.
- A provider identity cannot modify broker grant or audit state.

### Provider dogfood

Both Claude and Codex must complete paid opening and resumed supervisor turns.
For each provider, the attested manager must open a draft PR for a descendant
assignment and finish that assignment, while the adversarial sibling attempts
the same action, attempts `/mcp` send and resend against the manager, and fails.
Restart and revocation must be exercised during the dogfood, and the audit must
explain every accepted and denied request.

Only after those proofs may a provider-backed supervisor role be shipped or
restored. Passing on one provider, one OS backend, or a fake process does not
enable untested combinations.

## Consequences

This decision keeps human authority human-only and makes delegated scope small,
reviewable, and mechanically tied to durable assignment lineage. Reading the
ledger or discovering the endpoint does not become authority, and future
operator tools do not leak into delegation automatically.

The cost is real operational machinery: a launcher/helper, an attestation
backend, protected service data, lifecycle epochs, and provider-specific
dogfood. Some platforms may not support delegation initially. That complexity
is preferable to presenting bearer possession as process provenance.

Until the implementation and release gates pass, current behavior does not
change: provider-backed operator roles are refused, managers use the ordinary
agent surface to coordinate, and a human uses stdio or authenticated operator
HTTP to publish and finish work.
