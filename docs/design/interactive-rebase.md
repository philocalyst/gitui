# Interactive rebase: a persistent visual workspace

Design proposal · 27 September 2026 · repository baseline `07aeddd3`

**Make rewriting history feel like editing a visible, recoverable sequence.** The user should always be able to locate the original history, understand the intended result, identify the current stop, and see the next valid action. The same workspace must explain a clean rebase, the fifth conflict, a failed signer, and an interrupted session with incomplete metadata.

This is a proposed product and engineering design, not implemented functionality. The companion [interactive state atlas](interactive-rebase-atlas.html) illustrates representative states using fictional repository data. Research observations are distinguished from design recommendations below. Research used three Luna agents covering terminal clients, desktop clients, and editor integrations/Git semantics; the repository analysis and synthesis were performed locally.

## Reading map

1. [Current foundations](#current-foundations)
2. [Product contract and visual language](#product-contract-and-visual-language)
3. [Complete user journey](#complete-user-journey)
4. [The difficult states](#the-difficult-states)
5. [Typed domain and command architecture](#typed-domain-and-command-architecture)
6. [Execution, persistence, and recovery](#execution-persistence-and-recovery)
7. [Repository integration](#repository-integration)
8. [Verification and delivery](#verification-and-delivery)
9. [Full refactor](#full-refactor-the-operation-architecture) and [original interaction mechanisms](#more-expressive-interaction-mechanisms)
10. [Research and sources](#research-and-sources)

## Current foundations

The checkout already has good building blocks: Ratatui, configurable commands and themes, line/hunk staging, commit inspection, a graph renderer, background read jobs, external editing, and terminal snapshot tests. Reuse their visual conventions and underlying primitives.

| Existing area | Observation in this checkout | Consequence for the proposal |
|---|---|---|
| [`asyncgit/src/sync/rebase.rs`](../../asyncgit/src/sync/rebase.rs) | `RebaseState` is only `Finished` or `Conflicted`; execution loops over libgit2 operations. | Introduce an explicit session domain. More strings around this enum cannot represent the product. |
| Same file | `RebaseProgress` carries a count, index, and optional commit; `get_rebase_progress` uses `open_rebase` and iterator access. | Audit observation for side effects; the new observer must never advance a rebase. |
| [`asyncgit/src/sync/state.rs`](../../asyncgit/src/sync/state.rs) | Only `RepositoryState::RebaseMerge` maps to `RepoState::Rebase`; other variants fall into `Other`. | Recognize supported interactive/apply states and distinguish rebase from `git am` and unrelated sequencer operations. |
| [`src/tabs/status.rs`](../../src/tabs/status.rs) | Rebase context is formatted as step/current-commit text; continuation and abort call synchronous wrappers. | Add persistent structured context and move potentially long mutations off the UI thread. |
| [`src/popups/branchlist.rs`](../../src/popups/branchlist.rs) | Branch rebase invokes the backend and switches to Status. | Route into a reviewed plan/session, preserving the selected target. |
| [`asyncgit/src/sync/reword.rs`](../../asyncgit/src/sync/reword.rs) | Signed and unsigned reword have different paths; signed non-HEAD reword rebuilds a chain and rejects certain merge histories. | Preserve existing behavior until the new engine proves signing and reword parity. |
| [`asyncgit/src/asyncjob/mod.rs`](../../asyncgit/src/asyncjob/mod.rs) | `AsyncSingleJob` replaces the queued next job. | Suitable for superseded reads; unsuitable for a queue of rebase mutations that must not disappear. |
| [`src/components/command.rs`](../../src/components/command.rs) | Availability and enabled state are booleans, with separate display strings. | Add typed disabled reasons and one shared action-policy projection. |
| [`src/watcher.rs`](../../src/watcher.rs) | A debounced watcher monitors the workdir. | Explicitly observe resolved per-worktree Git metadata as well; use polling/reconciliation where watcher delivery is insufficient. |
| [`asyncgit/src/graph`](../../asyncgit/src/graph) | Typed graph rows/connections already exist. | Reuse glyph/layout primitives; represent hypothetical commits with their own IDs, never invented object IDs. |
| [`src/gitui.rs`](../../src/gitui.rs), [`src/snapshots`](../../src/snapshots) | Ratatui `TestBackend` and `insta` snapshots exist. | Build the state atlas as fixture-driven terminal snapshots during implementation. |

The existing `RepoState::Clean` describes operation state, not necessarily an empty index or worktree. Keep these concepts separate in preflight and labels.

## Product contract and visual language

### Five questions the screen always answers

- **What am I changing?** Branch, selected boundary, pinned destination, and affected commits.
- **What will this become?** A projected result with explicit uncertainty.
- **Where am I?** Completed operations, current stop, remaining operations, and local-edit restoration.
- **What needs my attention?** A concrete file, message, command, decision, or repository condition.
- **How do I leave safely?** Save draft, leave running work, abort, inspect recovery, or hand off, as appropriate.

Short prose still belongs in the interface. Labels and one-sentence explanations complement structure. Paragraphs must not substitute for showing affected commits, paths, relationships, and available choices.

### One stable workspace

Introduce a contextual **Rebase** workspace/tab, present while drafting, running, paused, or reviewing a recent outcome. The session persists when the user visits Status, Log, or Files. A compact global strip says, for example, `Rebase · conflict · 2 files · Open workspace`.

The layout has four stable regions:

1. **Context header:** `feature/cache → main @ 91be420`; scope; phase; recovery status.
2. **Sequence rail:** chronological replay order, operation badges, grouping, and execution markers.
3. **Inspector:** projected result while planning; the actual work item while stopped.
4. **Action shelf:** primary valid action, supporting actions, disabled action explanations, and help.

Use whitespace and a single divider more often than nested boxes. Selection, current execution, and invalidity must be independently visible. A selected future row must never masquerade as the commit currently being replayed.

```text
 Rebase  feature/cache → main @ 91be420            Draft saved locally
 SCOPE  6 source commits    RESULT  4 planned commits    ↓ replay order
 ────────────────────────────────────────────────────────────────────
 SEQUENCE                              PROJECTED RESULT
   ○ PICK    a18c7e2  Cache skeleton      main @ 91be420
   ├ PICK    b46f2a9  Add cache TTL          │
   └ FIXUP   c39aa10  Handle expiry         ○ Cache skeleton
 ▸ ◇ REWORD  d861b72  Tests                 ◆ Add cache TTL [2 sources]
   × DROP    e59b271  Debug logging         ◇ Cache expiry tests
   ○ PICK    f92a88c  Document cache        ○ Document cache
                                        feature/cache [planned tip]
 ────────────────────────────────────────────────────────────────────
 [Enter Inspect] [Action…] [Move…] [Undo]               [Review & start]
```

Here the bracket means “contributes to this output commit,” not ancestry. The projected graph is labeled separately from the sequence. The real Log remains in its existing orientation; the Rebase sequence explicitly says **oldest first / replay order**.

### A small visual grammar

| Meaning | Shape/text | Color role | Behavior |
|---|---|---|---|
| Future operation | `○ PICK` | Neutral | Hollow node; no claim that output exists. |
| Completed operation | `✓ PICK` | Success | Actual rewritten object available for inspection. |
| Currently executing | `▶ PICK` | Accent | Pointer independent of selection. |
| Intentional stop | `Ⅱ EDIT`, `Ⅱ BREAK` | Attention | A planned pause, not an error. |
| Needs resolution | `! CONFLICT`, `! SIGN` | Attention | Focusable problem with a next action. |
| Cannot proceed safely | `■ BLOCKED` | Danger | Reason and recovery choices occupy the inspector. |
| Reword | `◇ REWORD` | Accent | Old/new message preview. |
| Squash/fixup group | Bracket into one output node | Group accent | Members remain inspectable and reversible in draft. |
| Drop | `× DROP` | Muted plus explicit badge | Remains visible; never vanishes on a single keypress. |
| Unknown observation | `? UNVERIFIED` | Neutral/attention | Communicates uncertainty without inventing a cause. |

ASCII mode uses `o`, `>`, `+`, `!`, `x`, `?`, and `|`/`+-`. Color is redundant. Do not use emoji or assume Nerd Fonts. Respect existing dark/light themes; add semantic rebase roles with defaults so older theme files still load. Avoid pulsing errors, decorative gradients, and animated row rearrangement. A running indicator is acceptable; reduced-motion mode freezes it.

### Responsive and accessible behavior

- **Wide, approximately 120+ columns:** sequence, inspector, and optional result graph. Preserve minimum useful diff width before adding a third region.
- **Standard, approximately 80–119 columns:** sequence plus inspector; switch inspector between Result, Patch, Message, Checks, and Activity.
- **Narrow:** one pane at a time, with a persistent phase/current-stop header and tab-like pane selector. Do not compress three panes into illegibility.
- **Very small:** retain branch, current state, primary action, and navigation; show a concise resize suggestion. Never panic, crop a destructive confirmation into ambiguity, or rely on hidden controls.
- Support grapheme-safe truncation, wide characters, tabs, RTL/control-character-safe display, long paths, and non-UTF-8 path identity. Truncated labels open a complete detail view.
- Provide a linear/text presentation with explicit labels and predictable focus, while acknowledging terminal screen-reader behavior needs testing with real users.
- Optional mouse selection can be additive; every interaction must remain available through configurable keys. Avoid binding correctness to mouse dragging.

### Keyboard and focus contract

Use GitUI's configured navigation and command shelf. Suggested action mnemonics are `p` pick, `r` reword, `e` edit, `s` squash, `f` fixup, `d` drop; final defaults require a key collision audit. Keep a discoverable `Action…` menu, since mnemonic memorization is optional. Reordering has explicit `Move before…`/`Move after…` commands as well as optional direct keys. This avoids dependence on terminal-sensitive modifier combinations.

`Enter` inspects/activates the focused control; it never starts a rebase merely because a list is selected. `Esc` closes the innermost view, then leaves the workspace; it never aborts Git. Search and text editing capture character keys. Draft Undo/Redo apply to plan edits only. After a popup closes, restore focus by stable entity ID. A state change may expose the stop inspector, but must not steal focus from active text input.

## Complete user journey

### 1. Enter and choose the exact scope

Offer **Rewrite from this commit…** in Log, **Rebase current branch onto…** in Branches, and **Open current rebase** globally when an operation exists. The first means *include the selected commit*; show its parent as the excluded boundary. Support “after this commit” as a separately labeled action. If a noncontiguous selection is made, preview the entire contiguous replay range; selected rows can be highlighted, but intervening commits cannot silently disappear.

A scope screen shows a small real graph: unchanged base, affected range, destination, and current branch. Pin objects on selection; retain the friendly ref label alongside its observed OID. Distinguish the boundary determining **which commits** from the destination determining **where they go**. Advanced `--onto`, root, fork-point, and merge-preservation choices live here with graph examples. Never silently choose flattening for a merge-containing range.

Detached HEAD gets an explicit detached-session presentation and an option to create a branch. Unborn and bare repositories receive an honest unsupported/empty state. Shallow history, replace refs, and missing objects are visible scope constraints. Destination ancestry and a changed upstream must be validated rather than inferred from branch names.

### 2. Build a draft without rewriting the repository

Populate the sequence oldest first. Inspect original patch, message, author, parents, refs, signature presence, and changed files without executing the plan. Search by subject, OID, author, or path. Virtualize long lists. A minimap can mark group boundaries, drops, and deliberate stops for hundreds of operations; it must not become another unreadable commit graph.

Every draft edit is reversible and autosaved. Rows retain stable `StepId`s across movement and change of action. The user can leave, reopen GitUI, and resume the draft. Dirty drafts restored against changed refs show a comparison and require revalidation.

| Operation | Interaction | Structural feedback | Validation |
|---|---|---|---|
| Pick | Choose Keep/Pick | One source to one projected output | Source exists and remains in the supported range. |
| Reword | Edit message with original alongside | Output node shows the new title; source stays intact | Message policy, encoding, editor completion. |
| Edit | Insert a deliberate stop after replay | Pause marker and “amend or split here” inspector | Explain later descendants will also be rewritten. |
| Split | Choose Split; compile to an edit stop | One source expands into pending child slots | Actual child contents are determined during the pause. |
| Squash | Select target among valid earlier group heads | Sources visibly converge; combined message preview | Cannot start a sequence or cross an unsupported barrier. |
| Fixup | Choose target, usually the preceding output | Same convergence; target message retained | Preserve distinct `fixup`, `fixup -C`, and `fixup -c` semantics. |
| Drop | Mark row dropped | Ghost row and explicit removed-output count | Never silently drop omitted imported instructions. |
| Reorder | Move row/group with insertion cursor | Result order changes immediately | Validate groups, label references, and execution barriers. |
| Break | Insert between operations | Small intentional stop node | No source commit required. |
| Exec | Advanced command card | Command displayed verbatim as an operation | Explicit opt-in to command execution; never executed in preview. |
| Autosquash | Propose a batch of moves/groupings | Before/after proposal with accept/reject | Handle ambiguous/missing targets explicitly; no subject-only guess. |

Moving a group head moves its members by default. Moving a member out offers “detach as pick” or select a new compatible group; it does not silently retarget. Multi-select operations summarize the exact set. Invalid imported drafts remain renderable with row-level errors; validity is not required just to open a draft.

For merge-preserving plans, offer a **topology lens** showing labels, resets, and recreated merges. Basic mode collapses valid blocks but cannot pretend a dependency graph is freely reorderable. Raw-todo editing is an advanced round-trip view with validation, not the core experience.

### 3. Preview with honest confidence

The default preview is **structural**: ordering, grouping, message intent, drop decisions, ref impact, and expected deliberate stops. Use `planned` placeholders rather than fake hashes. Do not label a speculative combined diff as the eventual conflict-resolved result.

Three explicit preview levels:

| Level | What it establishes | What it does not establish |
|---|---|---|
| Structural | Instructions, relationships, known affected refs, projected output groups | Conflict freedom or final trees. |
| Content rehearsal, optional | Result of a controlled replay for the pinned inputs and supported policies | Behavior of omitted hooks, signing, exec commands, external filters, or future resolutions. |
| Observed result | Actual committed objects and recorded rewrite evidence | Semantic correctness of the code. |

Content rehearsal is a later feature, not a prerequisite for the first safe release. Run it only in an isolated disposable repository with controlled configuration and no implicit hooks, signing, exec commands, network access, or external filters. A linked worktree alone is insufficient isolation: refs and configuration can be shared. Record skipped mechanisms and invalidation inputs. If fidelity is insufficient, say “content preview unavailable.” Never run user commands merely to draw a green check.

For message-only/reorder/group edits, offer a before/after correspondence view. When the destination differs, separate destination changes from rewritten changes. The eventual review includes tree diff and range-diff with different labels; a range-diff match is a heuristic, not proof of semantic equality.

### 4. Review and start

A structured review replaces a generic “Are you sure?” prompt:

```text
 Rewrite feature/cache
   Range              a18c7e2 … f92a88c  [6 source commits]
   Destination        main @ 91be420
   Result             4 planned commits · 1 fold · 1 drop
   Deliberate stops   none
   Local changes     2 modified paths → protection required
   Affected refs      feature/cache; release tag stays on old commit
   Shared history    found in origin/feature/cache [local observation]
   Signing           required by effective configuration
   Recovery          will create original-tip checkpoint before replay

   [Protect changes…]                     [Start: blocked]
```

Preflight cards are actionable rows, not a wall of warnings. Relevant checks include repository operation, object availability, dirty index/worktree, untracked collisions, submodules, worktree ownership, identity/signing configuration, plan grammar, supported options, helper availability, storage permissions, and observed ref changes. Checks have `Pass`, `NeedsDecision`, `Blocked`, or `Unknown` results.

Never assert that history is “unpublished” just because local remote-tracking refs do not contain it. Show the observed evidence and freshness; a fetch is a user action, not a hidden prerequisite. Tags remain on old objects unless the user explicitly arranges a separate operation. Updating stacked branches requires a visible ref list and a supported `update-refs` policy.

Offer **stash tracked changes**, **include untracked files**, or **return to Status**, as explicit choices. Explain exclusions, including ignored files and nested repositories. Persist the exact stash object identity, not a mutable `stash@{0}` position. Blocking is preferable when the selected protection method cannot preserve the relevant state.

Start is a single deliberate action. Revalidate the pinned input immediately before mutation. Do not ask for additional generic confirmations after the concrete review; ask only if new material facts invalidate it.

### 5. Run in the same visual frame

As execution advances, hollow source markers become completed markers with actual output links. The current operation pointer moves along the existing rail. Group members remain grouped. The inspector displays current operation details and an expandable activity log; raw stderr is supplementary evidence.

Use “3 operations completed · applying 4 · 2 remaining,” not a fabricated time estimate. Source commits, todo instructions, output commits, and restoration tasks have different counts. A split or merge plan can change totals. Unknown totals stay unknown. “Completed” on a source action does not always mean a permanent final commit exists, especially during a squash chain.

Future plan editing is allowed only when the sequencer is stopped at a verified editable boundary and the runner is inactive. A live command's tail is read-only. Inspecting any row must not disturb execution. If the user browses away, the global strip preserves awareness.

### 6. Resolve a conflict as a work item

```text
 Rebase feature/cache → main       2 done · step 3 stopped · 3 later
 ────────────────────────────────────────────────────────────────────
 SEQUENCE                         RESOLVE · Handle expiry
   ✓ Pick   Cache skeleton        FILES                       STATUS
   ✓ Pick   Add cache TTL         ! src/cache.rs               2 regions
 ▶ ! Fixup  Handle expiry         ✓ tests/cache.rs             staged
   ◇ Reword Tests                 ! assets/cache.bin           choose file
   × Drop   Debug logging
   ○ Pick   Document cache        [Rebased so far] [Replayed patch]
                                  [Ancestor]       [Result]
                                  …focused resolution view…
 ────────────────────────────────────────────────────────────────────
 [Resolve file] [View original patch]      [Continue: 2 files unresolved]
 [More: skip / abort / recovery / diagnostics]
```

Name conflict sides **Rebased so far** and **Commit being replayed**, with OIDs and branch context. During merge recreation or other special cases, derive labels from actual merge inputs. Avoid unqualified “ours/theirs.” Git documents the reversal of their usual intuition during rebase. [Git rebase reference](https://git-scm.com/docs/git-rebase)

For text conflicts, show source panes and a result pane. Narrow terminals switch pane lenses. Support choose hunk, choose lines, edit result, external editor, and external mergetool. “Accept both” is a text operation available only when its construction is defined; do not treat it as a universal semantic resolution. Preserve edits and scroll position when re-observing.

Distinguish unresolved index entries, edited-but-unstaged files, staged resolutions, and warnings about remaining marker-like text. The index determines Git's unresolved status; marker scanning is only a review aid. Binary, rename/delete, modify/delete, symlink, file-mode, case-collision, and submodule conflicts get type-specific decision cards with exact paths and object IDs. Do not funnel them into a broken text editor.

Reuse existing staging facilities in a scoped workflow. After the final resolution is staged, Continue becomes available; it does not automatically execute. Revalidate on activation. A new conflict at the next commit updates the work item while retaining the surrounding journey.

### 7. Edit, split, and compose messages

At an edit stop, show the original commit, current rewritten commit, staged changes, unstaged changes, and result children. The primary path offers **Amend this commit**, **Split into commits**, or **Continue unchanged** when valid. External edits are observed and attributed conservatively.

The split view reuses hunk/line staging and adds a **remaining patch** ledger. The user builds child commits one by one. Show `Original contribution → child A + child B + remaining`. Finishing verifies remaining work and compares the resulting tree with the recorded starting tree. Intentional content changes are allowed after a visible review; do not enforce mathematical equivalence when the user intends an edit. Empty children require an explicit decision. Recoverability must survive a crash between child commits.

For reword/squash prompts, use a persistent message composer with source messages available alongside, validation, external editor support, and clear Save/Cancel semantics. Message cancellation pauses or returns to editing; it does not silently accept a default message. Save applies to the request that opened the composer, identified by a stable request ID and expected generation.

### 8. Finish history, restore local changes, then review

Separate **history rewritten** from **local changes restored**. If stash restoration conflicts, the rail's history portion is complete and the inspector becomes a restoration task. Never offer `rebase --continue` for a rebase that has already ended.

The outcome view shows original tip, final tip, source-to-result correspondence with evidence labels, dropped/skipped operations, affected refs, content comparison, changed messages, and unresolved review checks. Correspondence is `Exact`, `Observed group`, `Inferred`, or `Unknown`; there is no universal exact map after arbitrary external amendments, exec commands, and splits. Signature status is per actual output object; a rewritten signature cannot be portrayed as the original author's signature. Author and committer identity remain distinct.

Provide **Inspect result**, **Create recovery branch from original**, **Restore original history…**, and **Close**. A direct restoration is gated by observed branch/worktree changes and preserves new work first. Restoring into a new branch is the safe fallback when exact reversal cannot be proven. Draft Undo must not share a label with this operation.

Publishing remains separate. If the rewritten branch was shared, explain the divergence and offer to open the existing push workflow; never auto-force-push. Any later lease must be against an explicitly observed remote expectation and the push workflow's own checks.

### 9. Abort, leave, quit, and reopen

| Action | Product meaning | Required presentation |
|---|---|---|
| Leave workspace | Keep draft/session; navigate elsewhere | Global session strip remains. |
| Exit GitUI | Stop viewing; handle any owned child according to the verified supervision contract | Never pretend an active hook or command has been safely stopped. |
| Abort rebase | Ask Git to return to its original rebase state | Show original branch/tip and treatment of current resolutions/local changes. |
| Quit rebase | Remove rebase bookkeeping while retaining current HEAD/index/worktree | Advanced action, visibly distinct from Abort and application exit. |
| Skip current patch | Discard the current replay contribution and continue | Show exact patch/group effect, not a generic Skip label. |
| Reopen after crash | Reconcile persisted intent with observed Git state | Show restored context, mismatches, or uncertain ownership before enabling writes. |

Git's abort and quit behaviors differ; the interface must preserve that distinction. [Git rebase reference](https://git-scm.com/docs/git-rebase)

## The difficult states

This table is the acceptance inventory. Each row needs an observation fixture, an action-policy case, and a visual fixture before the corresponding capability ships. Unknown cases converge on an inspectable, non-mutating fallback; exhaustive enum matching cannot make external Git state exhaustive.

| Situation | What appears | Allowed next step / essential invariant |
|---|---|---|
| No commits in range / already up to date | Empty result with scope graph | Change scope or close; no error toast pretending failure. |
| Invalid draft, first-row squash, orphan fixup | Error attached to row/edge | Repair plan; Start unavailable. |
| Drop every source commit | Result is destination with no new commits | Explicit reviewed outcome; do not confuse with cancelling a todo editor. |
| Ambiguous autosquash target | Candidate target chooser | User selects; no arbitrary subject match. |
| Merge history selected | Topology choice and impacted graph | Preserve or explicitly flatten; capability-gated. |
| Root selected | Root badge, no fake parent | Supported root path or explicit limitation. |
| Detached HEAD | Detached destination identity | Preserve/recover detached result; optional branch creation. |
| Unborn/bare repository | Empty or unsupported operation card | No invalid worktree commands. |
| Shallow/partial repository, missing objects | Missing range/object identities | Fetch/deepen only through explicit action, then revalidate. |
| Dirty index and worktree | Separate staged/unstaged counts | Protect with declared policy; retain original staging intent where supported. |
| Untracked/ignored path collision | Exact collision list | Move/protect deliberately; never overwrite automatically. |
| Dirty/uninitialized submodule | Submodule status card | Handle separately; top-level stash is not a complete backup. |
| Sparse checkout unsupported by current app | Capability limitation | Inspect and hand off; do not claim full staging coverage. |
| Branch checked out in another worktree | Owning worktree path | Respect ownership; no force override. |
| Base/ref moves during draft | Old/new scope comparison | Regenerate against new base or retain explicitly pinned base. |
| Another operation is active | Merge/cherry-pick/am/rebase identity | Open/adopt only the matching supported operation. |
| Already-applied patch omitted by Git | Explicit omitted-source entry and reason | Review actual canonical sequence; no invented “completed” node. |
| Originally empty commit | Empty-source badge | Honor reviewed keep/drop policy. |
| Commit becomes empty during replay | Decision with original patch and current tree | Keep empty, skip, or edit where supported; no automatic guessing. |
| Text conflict | File worklist and resolution panes | Stage, re-observe, then Continue. |
| Rename/rename, rename/delete, directory/file | Path relationship diagram | Resolve paths plus contents; avoid unsafe generic choose-side action. |
| Binary/symlink/mode conflict | Object/mode chooser | View/external tool or explicit selection; no fabricated textual merge. |
| Submodule conflict | Before/rebased/replayed gitlink objects | Choose or resolve submodule commit; inspect separately. |
| Conflict resolved externally | Worklist transitions to staged/unstaged | Update without discarding local editor text or stealing focus. |
| Reused recorded resolution | “Reused resolution” evidence and diff | Review actual index; don't invent evidence that rerere ran. |
| Edit stop unchanged | Intentional pause marker | Continue unchanged is valid if backend permits. |
| Split partially completed | Child commits plus remaining patch | Resume same split session; no duplicate children. |
| Message editor fails/cancels | Composer request retained | Reopen, use another editor, or leave paused. |
| Exec nonzero exit | Command, exit status, output, next-position evidence | Retry/reschedule/continue only according to verified semantics. |
| Exec or hook edits files | File delta beside command result | Review dirty state; never erase side effects on retry. |
| Exec status ambiguous after crash | “Command outcome unknown” | Never automatically rerun a potentially non-idempotent command. |
| Hook fails, identity missing, signer unavailable | Specific evidence if available; otherwise command failure | Fix configuration and retry at the verified boundary. |
| Pinentry/credential/mergetool waiting | Waiting-for-external-tool view | Foreground handoff where needed; responsive cancel/navigation. |
| Lock file / live writer | Busy state with known ownership or unknown owner | Wait/reinspect; never automatically delete locks. |
| Disk full / permission failure | Storage problem and last durable checkpoint | Free/fix storage; re-observe before retry; don't claim a save succeeded. |
| Process interrupted mid-step | Reconciling state, then observed stop or uncertainty | Git state wins over stale app progress. |
| GitUI crashes with child still alive | “Operation still running” if verified | Observe only; never launch a second Continue. |
| External tool edits todo | Plan delta with changed/unknown instructions | Reparse, invalidate capabilities, preserve bytes. |
| External tool continues/aborts/finishes | Updated actual history and session outcome | Do not act on old current-step identity. |
| Metadata missing/malformed/truncated | Recovery inspector with raw evidence | Disable unsafe mutation; offer supported handoff and checkpoint inspection. |
| Unknown todo instruction / newer Git semantics | Opaque, preserved instruction | No automatic rewriting of an unsupported plan. |
| Rebase succeeds; stash restore conflicts | History complete + separate restoration worklist | Restore local changes; no rebase continuation command. |
| Abort itself fails | Abort failed, current observed repository state | Keep recovery points and evidence; no reset-hard fallback. |
| Final ref changed concurrently | Expected/actual ref comparison | Refuse blind overwrite; recovery branch or reconciliation. |
| Final hook fails after refs changed | Actual ref result plus hook failure | Do not label the whole operation rolled back. |
| Reopen after completed session, later commits exist | Prior outcome with current branch divergence | Recovery branch first; no unguarded “undo rebase.” |
| Backup/journal unavailable | Recovery capability shown as unavailable | Block promised managed execution or offer explicit external handoff. |

### A worst-case walkthrough

Consider: a user starts with staged edits; protects them; encounters a rename/delete conflict inside a fixup group; resolves it; a signer fails; GitUI exits; an external terminal amends the commit; GitUI reopens; the remaining replay succeeds; restoration of local edits conflicts.

The workspace must show one continuous story:

1. The original-tip checkpoint and local-edit stash appear as separate recovery items.
2. The group stays visibly bracketed when its second source conflicts.
3. Resolving paths changes the worklist but does not advance the sequencer.
4. Signing failure retains the same group and shows the actual command outcome.
5. Reopening first reconciles. The externally amended object is recognized as observed state; attribution is marked uncertain if exact correspondence cannot be established.
6. Continue is recomputed from the fresh snapshot, not resurrected from yesterday's enabled button.
7. Final history becomes complete; local-edit restoration becomes the only active task.
8. The final review remains available until restoration is resolved or explicitly left pending. Original and new histories both remain reachable.

No generic error popup can substitute for any of these transitions.

## Typed domain and command architecture

### Separate intention, observation, and presentation

Use three models with explicit conversions:

- **Plan:** what the user intends to do, including invalid or incomplete draft instructions.
- **Observation:** what Git, refs, index, worktree, and the owned process actually report.
- **Projection:** view data and permitted actions derived from plan + observation + durable intent.

A formatter cannot determine repository state. A toast cannot be the only storage for a failure. The renderer cannot invoke Git. Raw diagnostics are preserved separately from typed classifications.

Illustrative Rust, deliberately not a drop-in API:

```rust
struct SessionId(Uuid);
struct StepId(Uuid);
struct CommandId(Uuid);
struct Generation(u64);
struct PlanRevision(u64);
struct RepoFingerprint { /* refs, HEAD, index, todo, worktree evidence */ }

enum ReplayAction {
    Pick,
    Reword { message: MessageDraft },
    Edit { intent: EditIntent }, // Amend or Split
    Squash { target: StepId },
    Fixup { target: StepId, message: FixupMessagePolicy },
    Drop,
}

enum Instruction {
    Replay { source: CommitId, action: ReplayAction },
    Break,
    Exec { command: ShellCommandBytes },
    Label(LabelId),
    Reset(LabelOrOnto),
    Merge(MergeInstruction),
    UpdateRef(RefName),
    Opaque { original: Vec<u8>, reason: UnsupportedSyntax },
}

struct DraftStep {
    id: StepId,
    instruction: Instruction,
    origin: SourceLocation,
}

enum SessionPhase {
    Draft(DraftPlan),
    Preparing(PreparationState),
    Active(ActiveSession),
    Restoring(RestorationState),
    Finished(Outcome),
    Reconciling(ReconciliationState),
    RecoveryRequired(RecoveryState),
}

enum ExecutionStatus {
    Running(RunningCommand),
    Stopped(StopContext),
}

enum StopReason {
    Conflicts(ConflictSet),
    Edit(EditContext),
    Break,
    MessageRequired(MessageRequest),
    BecameEmpty(EmptyDecision),
    CommandFailed(CommandFailure),
    Unknown(ObservationEvidence),
}

struct StopContext {
    reason: StopReason,
    blockers: Vec<Blocker>, // multiple simultaneous conditions are possible
    at: ObservedPosition,  // not necessarily a commit or a known row
}

enum Availability<T> {
    Enabled(T),
    Disabled(NonEmpty<Blocker>),
    Hidden,
}

struct ActionPermit {
    session: SessionId,
    generation: Generation,
    plan_revision: PlanRevision,
    expected: RepoFingerprint,
    action: PermittedAction,
}
```

Keep fields and constructors private where they protect invariants. An `ActionPermit` is evidence to revalidate at dispatch, not an infallible token against external processes. Data loaded from disk is untrusted input even when written by an older GitUI version.

The sketch's payload types carry the details rather than hiding them in strings. `CommandFailure` has evidence-qualified `Hook`, `Signing`, `Editor`, `Exec`, `Storage`, `Process`, and `Unclassified` variants. Repository-integrity, ownership, or observation failures can move the session into `RecoveryRequired` instead of pretending to be an ordinary sequencer pause. Command exit and repository effect are separate axes: `NoObservedChange`, `PartialChange`, `RefsUpdated`, or `UnknownEffect`. A nonzero exit does not prove no mutation occurred.

`SplitContext` records the source object/tree, expected replay parent, split starting tree, managed child OIDs, remaining patch/tree observation, current staged/unstaged state, next message draft, and external-attribution status. `ObservedPosition` distinguishes not started, known instruction, and unknown position; absence must not become index zero.

Fold targets are semantic UI relationships. Compilation must place squash/fixup sources in a legal positional sequence; Git cannot execute a fixup into an arbitrary distant `StepId`. Merge instructions carry original message source, message policy, and parent/label dependencies. Reset targets preserve the supported original syntax. Exec carries raw command bytes, execution environment/cwd policy, and whether the user authored or explicitly adopted it. UI-generated removal uses explicit Drop instructions so omission is not confused with missing-commit checks.

### Important type boundaries

| Concept | Required distinction |
|---|---|
| Commit identity | Existing object ID vs projected output ID; no fake OIDs. |
| Operation identity | Stable step identity vs list index vs backend todo position. |
| Plan validity | Renderable `DraftPlan` vs validated, version-bound `ExecutablePlan`. |
| Source mapping | Exact observed group mapping vs inferred similarity vs unknown correspondence. |
| Stop | Primary stop reason plus independent blockers, not one mutually exclusive mega-enum for every combination. |
| Failure | Command failure evidence vs repository outcome; nonzero exit need not imply no writes. |
| Counts | Source count, instruction progress, output count, file-resolution count. |
| Recovery | Original-history checkpoint vs dirty-work snapshot vs resolution checkpoint. |
| Repository identity | Common Git dir + per-worktree Git dir + worktree identity; not a display path string. |
| Ref | Validated ref name + expected OID; not arbitrary shell interpolation. |
| Path/message | Raw identity bytes vs safe terminal display string. |
| Persistence | Versioned DTOs vs internal domain types; migrations are explicit. |

Do not expose a hundred loosely related booleans such as `is_conflicted`, `can_continue`, and `did_finish`. Derived projections can contain convenient booleans for rendering, but they come from one policy function.

### One command policy

`derive_actions(snapshot, session)` returns typed actions with availability and reasons. The footer, help menu, context menu, keyboard handler, and confirmation sheet all consume it. Keyboard dispatch still re-observes and checks preconditions before writing.

Examples:

- Conflicts present: `Continue` disabled with exact unresolved paths.
- No unresolved index entries but files awaiting staging: Continue depends on the backend's actual stop contract, not merely a “0 conflicts” count.
- Intentional edit with no changes: Continue unchanged may be enabled.
- Git finished but local restoration failed: only restoration actions appear.
- Unknown external operation: inspection is available; unsupported mutations are disabled.
- A runner is active: read actions remain available; a second mutation is rejected.

### Pure reducer, explicit effects

```text
Key / menu / watcher / child exit
                │
                ▼
      Typed event + generation
                │
                ▼
     Reducer ────────► View projection ───► Ratatui
        │
        ▼
   Validated effect
        │
        ▼
Per-repository mutation coordinator
        │
        ▼
 Git command / journal / editor bridge
        │
        ▼
 Re-observe repository → typed event
```

Read requests can be coalesced/cancelled and return versioned snapshots. Mutation requests cannot use the overwrite-next behavior of `AsyncSingleJob`. Use a serialized coordinator with explicit accepted/running/finished/uncertain command states, bounded queues, and duplicate-click rejection. Prefer rejecting an obsolete queued intent over applying it to a different stop.

A UI reducer must remain usable while a signer, hook, or Git process waits. Backpressure may coalesce progress messages but must not discard completion or error outcomes. Discard late read results whose generation no longer matches. Watcher events are invalidation hints, not authoritative transaction boundaries.

## Execution, persistence, and recovery

### Backend decision

**Recommendation: use Git's native interactive sequencer for managed execution, behind a narrow adapter; retain libgit2/gix for suitable reads.** Validate this with a short architecture spike before making the UI depend on it.

| Approach | Strength | Principal cost | Recommendation |
|---|---|---|---|
| Extend current libgit2 loop | Familiar integration, useful typed APIs | Must establish parity for todo editing, merge recreation, hooks, signing, empty commits, external sessions, and recovery | Keep existing operations until proven replacement; don't assume parity. |
| Native Git process + observation adapter | Executes the user's Git semantics and supports shell interoperability | Process/editor supervision and version-aware observation are substantial work | Preferred new engine. |
| Implement a new sequencer in Rust | Maximum internal control | Reimplements mature, evolving Git behavior and its crash semantics | Avoid for the initial architecture. |

Libgit2 exposes rebase primitives, but API availability alone does not establish complete interactive workflow coverage. [libgit2 rebase API](https://libgit2.org/docs/reference/main/rebase/index.html)

### Adapter contract and capability negotiation

Define `observe`, `prepare`, `start`, `continue`, `skip`, `abort`, `quit`, `edit_remaining`, and message/tool handoff operations. Each returns command outcome plus a fresh observation; never only `Result<()>` as the product result.

At startup establish Git executable identity/version and supported option/backend combinations. Implement a capability table tested across supported versions, not ad hoc feature optimism. Make minimum version a release decision after the spike. Root, merge preservation, update-refs, fixup message flags, object formats, and alternate backends must be independently gated. An unavailable advanced feature does not hide the whole workspace.

Use argument arrays and scoped environment overrides. User-supplied refs and paths are validated/separated from options; display strings are never command fragments. `exec` intentionally contains shell code and is a different type with explicit user authorization. Preserve effective config that affects semantics; expose intentional overrides instead of silently neutralizing user policy.

### Plan generation and the editor bridge

There is no assumed stable `git rebase --dry-run --json` API. Generate a read-only draft from repository history and supported policy; treat Git's eventual canonical todo as authoritative. Implement a small same-executable helper mode for `GIT_SEQUENCE_EDITOR` and `GIT_EDITOR`, with per-command session/request tokens and IPC or narrowly scoped files.

At Start, the sequence-editor helper receives Git's generated todo before replay. Compare its operations with the reviewed plan. If the difference is material, stop before replay and return to review; account for any preparation/autostash effects already performed. Do not silently add omitted commits or discard Git's exclusions. The adapter spike must verify the exact failure/cleanup behavior on every supported backend/version.

Treat this as a required adapter contract to prove, not an already established guarantee: prevent unreviewed replay through editor failure and reconcile every preparation side effect. If a backend/version cannot satisfy that contract, do not enable managed Start for it. A failed editor is not evidence that no state, stash, lock, or temporary ref was created.

The bridge needs a concrete protocol: request ID, session ID, expected generation, request kind, file identity, raw bytes, response, cancellation, and timeout. Verify paths belong to the expected Git request; reject stale/replayed tokens. Store message drafts before acknowledging save. Helpers must fail clearly when GitUI disappears, leaving a recoverable Git stop rather than hanging forever or accepting an empty message.

Unlike ordinary Git argv, editor environment variables are command strings interpreted using platform-specific execution rules. Use a tested helper-launch encoding and pass session tokens through environment/IPC rather than interpolating user text. Fixture-test executable paths with spaces, shell metacharacters, Windows quoting, terminal suspension, and helper disappearance. Never forward raw hook/process output directly as terminal escape sequences; render sanitized diagnostic text and retain raw bytes separately with local access controls.

Do not insert executable instrumentation into the user's todo to fake progress. Do not overwrite `post-rewrite` hooks to collect mappings. Native Git owns sequencer files; writes to todo occur through the supported edit-todo/editor interaction while stopped. Unknown/comment bytes are preserved by a lossless parser/serializer, and unsupported syntax blocks automatic plan mutation.

Byte preservation applies to the application's retained input and its own unchanged spans. Git may regenerate or normalize its sequence. Keep the original bytes for comparison; do not promise that Git preserves comments or formatting. After raw editing, reparse and invalidate the previous plan revision before accepting new typed actions.

### Observation without parsing random prose

Use structured ref/index/status information wherever available. Git's porcelain status formats are designed for parsers; use NUL-terminated paths and model unmerged entries explicitly. [Git status reference](https://git-scm.com/docs/git-status)

Rebase metadata is more delicate: internal files are not a universal versioned API. Encapsulate supported layouts in tested adapters. Read process lifecycle, sequencer state, index, refs, and relevant files into a consistent-enough observation; if they change during the read, retry within a bounded limit or mark the snapshot unstable. Resolve Git paths through repository discovery rather than assuming `.git` is a directory.

Never classify by matching localized stderr as the sole truth. Retain stdout/stderr, exit status, and command identity for diagnostics; derive specific failures only when supported evidence exists. `Unknown` is a valid first-class state. Preserve uncertainty about which instruction ran if the evidence cannot distinguish it.

Git's sequencer implementation is useful compatibility evidence, not a public contract. Pin source/test references to the supported release during implementation. [Git sequencer source](https://github.com/git/git/blob/master/sequencer.c)

### Persistence design

Store application-owned session metadata under a resolved per-worktree Git metadata location, such as `<git-dir>/gitui/rebase/<session-id>/`; resolve actual paths for linked worktrees. Common refs and worktree-local state must not be conflated. [Git worktree reference](https://git-scm.com/docs/git-worktree)

Persist:

- Schema version, repository/worktree identity, session ID, creation/update times.
- Original branch/detached HEAD, original tip, pinned boundary/destination, relevant ref observations.
- Draft plan with stable IDs, undo/redo history or compact checkpoints, and validation revision.
- Effective backend options/capabilities relevant to replay.
- Recovery refs and separately recorded local-edit protection identity/policy.
- Command intent ID, expected fingerprint, invocation state, and last observed outcome.
- Pending editor request/draft, split context, and observed correspondence evidence.
- Latest stable observation and bounded diagnostic/activity references.

Use atomic replace, flush/fsync where supported, explicit failure reporting, and corruption detection. Separate durable state from disposable caches. Validate size limits and schema on load; unknown future schema opens a recovery view, not destructive defaults. Keep prior valid state when an update fails.

**Git remains authoritative for execution progress.** The journal restores user intent and helps reconcile; it must never replay Git mutations just because a persisted command lacks a completion record.

### Recovery refs and dirty-work protection

Before a managed rewrite begins, create a namespaced ref to the original tip, for example `refs/gitui/rebase/<session>/original`, and record any other refs whose updates are authorized. Verify ref creation before promising recovery. For an externally started rebase, create a checkpoint only from verified available origins and label incomplete coverage honestly.

This example deliberately uses a shared ref namespace, with a globally unique session ID and explicit owning-worktree metadata. It is not worktree-local. Shared reachability is useful when recovering after a worktree is removed; creation and cleanup must be coordinated across GitUI worktrees. A per-worktree namespace is an alternative, but must not be silently substituted without considering its lifecycle. Recovery refs are ordinary local refs: mirror pushes and explicit broad refspecs can include them. Keep them outside normal branch/tag selectors and never advertise them as private or unpushable.

Use expected-old-value ref operations for compare-and-swap and transactional updates where supported. Git's `update-ref` provides these primitives. [Git update-ref reference](https://git-scm.com/docs/git-update-ref)

A ref protects committed objects from ordinary reachability-based collection; it does not protect unstaged changes, untracked files, index conflict stages, hook side effects, or remote state. Track those separately. Do not call an original-tip ref a “complete backup.” Retain named recovery points until explicit cleanup or a visible retention policy; failed/unfinished sessions must not be purged silently.

For automatic local-edit protection, prefer an app-managed stash flow with object-ID tracking and explicit `apply`, verification, then optional removal; avoid a blind positional `pop`. Restoration must preserve the recorded staged/unstaged policy where supported, and any limitation belongs in preflight. Recovery of conflict resolutions requires a separate tested snapshot mechanism capable of preserving unmerged-index information; ordinary stash cannot be assumed to handle it.

### Crash protocol and concurrency limits

For every mutating command:

1. Acquire the GitUI mutation lease for the repository/worktree and coordinate shared-ref effects with other GitUI sessions.
2. Re-observe and validate the permit's inputs. External Git tools do not honor the GitUI lease.
3. Durably record **intent** before spawning; if that fails, do not mutate.
4. Start one supervised child; record process identity beyond a reusable PID where possible.
5. Collect process outcome and re-observe actual Git state, including nonzero exits.
6. Durably record the outcome. If this write fails, treat the next startup as reconciliation, not safe retry.

Git operations and sidecar writes cannot be made one cross-system atomic transaction. A crash can occur after Git changes the repo but before GitUI records success. On restart, reconcile rather than promise exactly-once execution. In particular, never auto-repeat `exec`, hooks, stash application, or commit creation when their outcome is uncertain.

Git's own locks arbitrate supported internal operations. They do not create a global lock respected by every external process across a multi-command workflow. Minimize race windows, pin OIDs, use compare-and-swap for refs, and revalidate before each effect. If external changes are detected, freeze mutation affordances, show the delta, and rebase the *UI's understanding* onto observed state.

Fingerprints use selective preconditions: HEAD and relevant refs, index identity/checksum where appropriate, todo contents, and relevant path observations. They are stale-state detectors, not proofs that every worktree byte stayed unchanged. Do not hash an entire giant working tree on each keypress or claim that mtime/size checks eliminate races. The final native command can discover hazards preflight did not observe.

Avoid a universal Pause button: safely pausing a Git process, hook, or arbitrary shell command is not generally equivalent to pausing between todo entries. Planned break instructions provide reliable boundaries. Cancel a running command only through a documented supervisor policy and then reconcile; interruption is not rollback.

### External-session adoption

Recognize an in-progress rebase started from a shell or another client. Initially offer observation and supported native Continue/Abort/Skip actions only when their preconditions and backend identity are known. Add remaining-plan editing once round-trip compatibility is proven. Never require GitUI's sidecar to understand that a rebase exists.

When exact source mapping is unavailable, render “observed after external change” and maintain object links. Do not match by subject or assume one old commit maps to one new commit. Squash is many-to-one; split is one-to-many; drop is zero; merge recreation involves parents; some correspondences stay unknown.

## Full refactor: the operation architecture

The requested scope is a complete refactor of the foundations supporting this experience. The end state removes the old rebase-specific synchronous mutation paths, boolean-driven action decisions, and string-only session status. Migration can be incremental; the final architecture must be coherent.

### A small operation core, with a substantial rebase domain

Introduce a shared `OperationCoordinator` for mutation ownership, observation generations, child supervision, persistence, and terminal handoff. Keep rebase instruction semantics in `RebaseSession`, conflict semantics in a shared resolution domain, and staging semantics in the existing staging layer after extracting a typed interface.

Do not put every Git operation into a giant generic state machine. Rebase, merge, cherry-pick, and revert can share repository observations, conflict work items, process supervision, and recovery primitives while retaining different commands and lifecycle rules. Rebase is the first complete consumer; broader migration happens after that contract is proven.

```text
App shell
  ├─ Workspace registry / focus / navigation
  ├─ Action registry → hints, menus, keyboard routing
  └─ Operation coordinator
       ├─ Observation service → versioned repository snapshots
       ├─ Mutation executor → single owned writer + Git processes
       ├─ Session store → intent, recovery, reconciliation
       ├─ Rebase domain → plan compiler + runtime interpreter
       └─ Shared work items → resolve / stage / compose / tool handoff

Views receive projections. Domains request effects. Only executors mutate.
```

The action registry is typed and contextual: `ActionId`, payload, label resource, keybinding, availability, disabled reasons, impact summary, and dispatch target. Migrate rebase first, then Status actions that can interfere with a session. During rebase, checkout, reset, amend, commit, stash, and branch/ref operations must consult session policy. A disabled rebase Continue button is insufficient if another tab can unknowingly invalidate the operation.

Some Status actions remain intentionally allowed: staging a conflict, editing files, committing a split child, or amending at an edit stop. They carry session context so the coordinator can distinguish a legitimate subtask from an unrelated mutation. External terminal commands remain possible and trigger reconciliation; GitUI cannot enforce its internal ownership rules outside its own process.

### Treat the plan as a small language

Build a lossless concrete syntax representation for imported todo bytes, a typed semantic plan for editing, and a compiled execution plan for the selected backend. This makes raw editing, visual editing, validation, and serialization different views of one program.

```text
Raw todo bytes ──parse──► Lossless syntax
                              │
                              ▼
User intent ───────────► Semantic plan
                              │ validate + lower
                              ▼
                      Executable instructions
                         │             │
                         ▼             ▼
                  Git todo output   Source map
```

The source map relates user intent to emitted instructions and projected groups. `Split` lowers to an edit boundary plus managed split intent; a proposed autosquash lowers to explicit moves/folds after review. Typed dependencies represent fold targets and merge label/reset references. This graph is a validation graph, not a promise to execute commits in parallel.

Add a type distinction between `PlanNode` and `RuntimeWorkItem`. A conflict, signer prompt, and recovery comparison are runtime work items; they must never be serialized as Git todo rows. One source instruction may generate several work items, and one work item can apply to a group. This avoids synthetic conflict rows corrupting todo indexing.

### Evidence-aware projections

Attach provenance to meaningful claims:

```rust
enum Knowledge<T> {
    Observed { value: T, at: Generation, evidence: EvidenceId },
    Derived { value: T, inputs: NonEmpty<EvidenceId> },
    Projected { value: T, plan: PlanRevision },
    Unavailable(UnavailableReason),
}
```

Do not display these Rust terms to ordinary users. Render “planned,” “observed,” “changed externally,” or “not yet known” where the distinction affects a decision. Evidence IDs also make diagnostics useful: selecting a blocked action can show the exact paths/ref/config observation behind it. Rendering the explanation and enforcing the command use the same evidence inputs.

### Refactor completion criteria

- No rebase widget calls a synchronous mutation function.
- No rebase action derives availability independently of the central policy.
- No user-visible phase is inferred solely from an arbitrary error string.
- No executable instruction is identified by rendered row index.
- No successful save/checkpoint indicator appears before durable persistence succeeds.
- No observer uses a mutating iterator to discover progress.
- No mutation bypasses the coordinator from another GitUI tab during a managed session.
- Legacy simple-rebase/reword routes are removed or explicitly delegated to the new engine after parity coverage.
- All active-session screens can be recreated from a fixture containing domain state and observations; they do not need a live Git process just to render.

## More expressive interaction mechanisms

These are original proposals built on the research, not claims about existing clients. Each improves a specific user decision and has a bounded implementation path.

### 1. A correspondence lens: see where a change goes

Press a lens key on any source commit to highlight its intended/observed result group while dimming unrelated rows. Squashes converge; splits fan out; drops end in an explicit removed marker. In wide layouts show source → transformation → result. In narrow layouts show the same relationship as a short stack.

Keep this lens opt-in: drawing every crossing connector at once would make reorder-heavy plans unreadable. Exact observed mappings use solid connectors; projected or inferred mappings use a different line pattern and label. A small “why this mapping?” detail exposes its evidence.

### 2. Draw the obstacle, then unfold its prerequisites

A conflict node expands into a small task tree:

```text
 Continue step 3
   ├─ Resolve src/cache.rs       2 regions remaining
   ├─ Choose assets/cache.bin    2 candidate objects
   └─ Review staged result      waiting for resolutions
```

This **next-actions view** is derived from blockers, not hand-written state copy. When one blocker clears, the branch closes and the remaining tasks stay in place. If signing then fails, it becomes the next work item on the same step rather than a disconnected error dialog. Unknown prerequisites stay unknown; no fabricated prediction of the next failure.

### 3. Preview a move before placing it

Moving a commit enters placement mode. An insertion cursor shows a tiny before/after list and affected fold/label relationships. Invalid positions show the exact dependency edge they violate. Confirm once to commit the draft edit, or Esc to leave it unchanged. Provide an explicit “move group” versus “detach member” choice.

Possible content dependency is advisory: touching the same file does not prove an order dependency, and touching different files does not prove independence. Do not invent a universal “safe reorder” score.

### 4. A local-work tray that survives every phase

Show a compact tray for the user's pre-existing changes: `Local work: protected · 2 paths`. It is distinct from the history checkpoint. Opening it shows paths, staged/unstaged policy, saved object identity, and restoration status. When replay completes, the tray becomes the active task if restoration needs attention.

This makes the most easily forgotten part of rebase visible throughout, without filling the sequence with non-commit rows.

### 5. Recovery as a comparison, not a panic button

The recovery view compares three named positions: **Before rebase**, **Last observed result**, and **Repository now**. It offers exact paths such as “create a branch at the original tip,” “inspect changes made since this session,” or “restore the original branch after protecting new work.” A return connector is drawn only for a supported action.

No rollback graph should imply that hooks, remote pushes, external programs, or unrecorded worktree edits can be reversed. Each recovery item shows its coverage.

### 6. Separate “rewrite” and “restore” progress

Use a two-stage lifecycle strip: `Rewrite history → Restore local work → Review`. Inside the first stage show operation counts. This makes a completed rebase plus a failed stash application visually comprehensible and avoids a misleading 100% success bar.

Intentional edit/break gates appear on the rail; unknown-duration commands use an activity indicator without an invented percentage. A future “advance one operation” control is implemented only by explicit, reviewed break boundaries with tested semantics, never by racing a signal against the Git process.

### 7. A visible plan-change ledger

Summarize draft edits as reversible chips: `Moved tests earlier`, `Folded expiry fix into TTL`, `Dropped debug logging`. Selecting a chip highlights the exact rows/edges and offers Undo through the draft command history. Each compound action has one understandable undo unit.

During execution the activity ledger is read-only evidence. Scrubbing it changes the inspected snapshot and shows a prominent historical-view label; it never rewinds the actual repository. “Return to live” is always available.

### 8. Build repeatable checks into the plan

An advanced “check after these outputs” command can add visible `exec` cards after selected output groups. Show command, working directory, and scope before acceptance. Place checks after a completed squash group, not blindly after every contributing source. Disabled or omitted checks remain visible in a proposal diff.

Tests are user-authored commands with ordinary shell side effects. Failure UI records actual outputs and retry semantics; preview never runs them automatically. No claim that a passing test proves the rebase correct.

### 9. One scenario catalog drives design, tests, and help

Store named `ScenarioFixture`s containing observations, plan, view preferences, and expected allowed actions. Use the same fixtures for terminal snapshots, an interactive development gallery, keyboard journeys, and focused help illustrations. Add combinations, not only one fixture per enum variant.

The supplied HTML atlas is an early visual sketch. The implemented catalog must use the actual Rust projection and renderer so screenshots cannot drift from real policy. This is a practical way to keep beautiful error states maintained rather than forgotten after launch.

### 10. Let users propose changes cheaply

Allow duplicating a saved draft into a second proposal against the same pinned inputs. Compare operation/group differences and choose one to execute. This explores “keep these separate or fold them?” without touching Git history. Both drafts invalidate when their required inputs change; neither is a sandboxed repository or a guarantee of conflict freedom.

Defer this until core draft durability is proven. The data model should support it without teaching the runtime to execute multiple competing sessions in one worktree.

### Two additional inspector lenses for difficult sessions

**Recovery coverage:** one consistent location lists actual objects and exclusions: original-tip ref, protected local edits and policy, optional resolution snapshots, hook side effects not captured, and remote observation age. Avoid a numerical safety score or a green “safe” badge. The header can link to this ledger, while the tray remains its compact local-work summary.

**Related future changes:** when resolving `src/cache.rs`, optionally highlight future source patches touching that path. Label this “shared path,” never “will conflict.” Rename tracking and unavailable objects can make overlap unknown. This helps users inspect relevant later work without predicting conflicts from a crude heuristic.

Use consistent verbs everywhere: **Pause to edit** changes a todo action; **Edit plan** changes future instructions; **Continue rebase**, **Restore local changes**, **Leave workspace**, **Quit Git rebase**, and **Abort rebase** each name a different operation. A checkmark means a recorded action completed, not that its code is correct. Tests, signature validation, and content review have separate results.

## Repository integration

Proposed file organization, adjustable to project conventions:

```text
asyncgit/src/rebase/
    mod.rs                 public session/adapter interface
    model.rs               plans, IDs, observations, outcomes
    validate.rs            plan invariants and capability checks
    policy.rs              permitted actions and typed blockers
    observe.rs             consistent snapshots and reconciliation
    todo.rs                lossless parsing and serialization
    native.rs              Git invocation and version adapter
    runner.rs              serialized mutation coordination
    journal.rs             persistence and migrations
    recovery.rs            refs, local-edit protection, restoration

src/tabs/rebase.rs          workspace composition
src/components/rebase/
    sequence.rs            instructions, groups, execution cursor
    scope.rs               boundary and destination graph
    result.rs              projected/observed correspondence
    stop.rs                stop dispatcher and shared frame
    conflicts.rs           worklist and resolver integration
    review.rs              preflight and completion review
    activity.rs            structured event ledger + diagnostics

src/rebase_bridge.rs        helper entry point / editor protocol
```

Integrate with `App`, `AppTabs`, `InternalEvent`, `AsyncNotification`, key configuration, strings, theme, and Status. Avoid introducing a separate application framework. Refactor shared diff/staging primitives narrowly when the workspace needs them; do not duplicate staging semantics. Existing Log graph primitives inform drawing, but source history and a future plan need different data models.

Route all rebase-related mutations through the coordinator once migrated. Keep current simple branch rebase/reword behavior until parity tests pass; don't run two backend engines inside one active session. Start with one observed session per worktree and explicit repository switching behavior. Multi-repository dashboards are out of scope.

## Verification and delivery

### Testing layers

1. **Domain/property tests:** stable IDs under moves, group referential integrity, label/reset dependencies, validation/serialization equivalence, preservation of opaque bytes, draft undo/redo, many-to-many correspondence, action-policy consistency.
2. **Reducer transition tests:** duplicate inputs, late reads, new generations, simultaneous blockers, external progress, message cancellation, terminal resize, exit/reopen, and unknown states. Assert commands emitted, not only strings rendered.
3. **Native Git fixture repos:** expected commit parents/trees/messages/refs for each supported instruction and option. Include original/became-empty distinctions, conflicts, root, merges, signing, hook failures, and external session adoption.
4. **Fault injection:** crash before/after journal write, spawn, commit, ref update, and stash restore; disk full; malformed metadata; truncated journal; process disappears; hook exits after modifying files. Assert no blind rerun and preservation of reachable history/local work within the declared guarantees.
5. **Terminal snapshots:** each acceptance state at standard, wide, narrow, and tiny sizes; dark/light/monochrome/ASCII; long and unusual names. Include selection different from current operation and focused message input during background changes.
6. **Keyboard journeys:** discover entry, create a group, undo a move, inspect preview, start, resolve, finish, restore, exit/reopen. Exercise remapped keys and text-input isolation.
7. **Compatibility:** declared Git versions, Linux/macOS/Windows, linked worktrees, SHA/object-format capabilities, hooks/editors with spaces, CRLF paths/messages, and differing local configuration. Unsupported formats/configurations get explicit capability fixtures.
8. **Performance:** large plans/repositories, huge diffs, watcher bursts, slow storage, and slow Git children. Ensure bounded rendering and logs, lazy details, cancellable reads, and responsive navigation.

Snapshots test presentation, not safety. Repository fixtures must inspect refs, objects, index, worktree, and durable state. Use fake signers/hooks in deterministic tests plus limited real integration coverage. Tests must run in isolated repositories and configuration, never the developer's active repository.

### Proposed performance and usability gates

These are targets to measure, not existing guarantees: ordinary navigation/render response under 50 ms at the 95th percentile on a documented reference machine; no UI-thread Git commands; a 1,000-step plan remains scrollable while details load; background results cannot change the selected row unexpectedly. Test users should identify the active stop and next action within five seconds in prepared scenarios. Measure novice and experienced Git users separately.

### Delivery sequence and exit criteria

| Milestone | Deliverable | Exit criterion |
|---|---|---|
| 0. Semantic spike | Native adapter experiment, bridge protocol, supported-version matrix, process-exit behavior | Demonstrate pick/reword/conflict/edit/empty/abort, external adoption, and crash-after-write reconciliation in isolated fixtures. Decide engine and minimum Git version. |
| 1. Observation and recovery foundation | Typed observations, journal, checkpoints, mutation coordinator; basic read-only workspace | Existing rebase is explained without changing it; unknown states remain inspectable; restart/fault tests pass. |
| 2. Draft composer | Scope selection, pick/reword/drop/reorder/squash/fixup, groups, undo, autosave, structural preview | Draft survives restart; validation catches invalid structures; no repository rewrite during composition. |
| 3. First managed vertical slice | Review/start, basic native execution, message bridge, conflict worklist using existing editor/staging, continue/skip/abort, completion | One complete recoverable journey, including crash and dirty-work restoration failure. Safe fallback for unsupported features. |
| 4. Rich resolution and editing | Integrated conflict views, edit/split, empty decisions, tool/signing/hook stops, future-plan editing | Difficult-state fixtures for each enabled feature pass across supported backends/versions. |
| 5. Advanced topology and policy | Merge-preserving lens, root, advanced fixup flags, autosquash, exec/break, selected update-refs support | Dependency validation and external round-trip coverage; explicit unsupported boundaries. |
| 6. Preview and polish | Optional controlled rehearsal, richer correspondence, accessibility, performance, onboarding | Confidence labels accurate; no hidden execution in previews; usability and performance gates met. |

Each milestone should break into reviewable PRs around one invariant or vertical behavior. Avoid shipping a beautiful editor with no credible recovery path. Also avoid making a full integrated merge editor a prerequisite for a useful first release: a structured worklist plus existing staging/external tools can satisfy the initial contract.

### Concrete refactor work packages

| Package | Change | Depends on | Review evidence |
|---|---|---|---|
| A | Introduce repository/worktree identity, observation generation, and operation identity without changing execution | Semantic spike | Read-only fixtures distinguish rebase, am, merge, and unknown operations. |
| B | Add the typed action registry and session-aware dispatch boundary | A | Menus, keys, and disabled reasons agree; stale dispatch is rejected. |
| C | Implement supervised native runner and editor helper protocol | A | Process/editor/signing/tool handoff fixtures, including disappearance and cancellation. |
| D | Add versioned session storage, original-tip refs, and reconciliation | A, C | Fault injection around intent/result writes; no duplicate effects after restart. |
| E | Build lossless todo syntax, semantic plan, validation, lowering, and source maps | A | Parse/serialize/property fixtures, unknown syntax, positional fold and topology constraints. |
| F | Create the pure view projection and fixture-driven Rebase workspace | A, B, E | Layout snapshots and keyboard navigation for draft, active, unknown, and recovery states. |
| G | Add composition commands, groups, draft undo, autosave, and structural preview | D, E, F | Complex draft restored without touching history; focus and stable IDs retained. |
| H | Join review/start, canonical sequence reconciliation, and basic lifecycle | B, C, D, E, F | Complete pick/reword/drop/fold journey with real Git and crash recovery. |
| I | Extract shared conflict/staging work items and integrate basic resolution | B, H | Conflict → external editor/staging → continue, with blocker and dirty-state checks. |
| J | Add protected-local-work lifecycle and honest completion/recovery review | D, H, I | Stash application failure, abort failure, concurrent ref changes, guarded restoration. |
| K | Route legacy branch-rebase/reword and interfering Status actions through the new boundaries | H, I, J | Behavior/signing parity; no old mutation path can bypass an active session. |
| L | Add edit/split/message and evidence-qualified command failure experiences | H, I, J | Crash between split children, failed signer/editor/hook, external amendment. |
| M | Add advanced topology, exec/break, autosquash, and selected ref-update policies | E, H, L | Capability/version matrix and cross-worktree tests for each enabled feature. |
| N | Add optional rehearsal, comparison drafts, richer lenses, accessibility, and performance polish | G–M as relevant | Confidence labels and isolation tests; representative user journeys and measured budgets. |

Packages may need multiple PRs; these are dependency and review boundaries, not an assertion that the work is small. Keep each executable backend behind a capability gate until its full vertical journey, including failure and reopening, passes. Remove obsolete rebase status/mutation code after package K rather than maintaining competing permanent systems.

### Release and maintenance contract

Start with a feature-gated internal gallery and isolated fixture execution. Then enable managed basic plans for the proven Git/backend matrix, followed by advanced capabilities individually. A feature flag must never make an already-active session invisible: observation and recovery remain available even if new-session creation is disabled. A downgrade encountering a newer journal schema preserves the files and provides read-only Git observation/handoff.

Document key remapping, scope selection, publication implications, editor handoff, and recovery coverage with the same fixture examples as the UI. Add a sanitized diagnostic export containing versions, instruction kinds, phase transitions, and evidence classifications; exclude file contents, commit messages, command output, and secrets by default. Exporting sensitive detail is an explicit local choice. No telemetry is required to operate the feature.

Maintenance includes rerunning the compatibility corpus when Git support expands, keeping opaque syntax preserved, and requiring a fixture whenever a new failure is reported. Retention cleanup removes only app-owned completed-session artifacts under a documented policy, never native sequencer files or live Git locks. If a session is uncertain, preserve it for inspection.

### Decisions to settle during milestone 0

- Exact native Git support range and policy for environments without a usable executable.
- Helper IPC transport and Windows process/terminal handoff behavior.
- Supported metadata layouts and how to recognize an unstable snapshot.
- Canonical todo capture/reconciliation and editor-failure cleanup guarantees.
- Supervision policy when the UI exits while a child is active.
- Extent of dirty-work protection and retention defaults.
- Feasible exact rewrite-mapping evidence without replacing user hooks.
- Whether basic and advanced merge workflows share one engine capability set at launch.
- Required accessibility behavior and representative terminal test matrix.

Everything else above is a proposed default, not an unanswered product questionnaire. The design is ready to prototype while these engineering facts are established.

## Research and sources

The following section records researched patterns and the recommendations they motivate. It does not claim that any one existing product already delivers the complete workspace proposed here.

| Application | Documented pattern | What to take into GitUI | Adaptation / caution |
|---|---|---|---|
| LazyGit | Commit panel becomes a todo/progress view, with a current-position marker and rebase menu. [Official walkthrough](https://github.com/jesseduffield/lazygit/wiki/Interactive-Rebasing) | Preserve context as an operation runs; offer focused shortcuts as well as a full editor. | Its documented display reverses todo order. Make GitUI's replay direction explicit and keep local commands discoverable. |
| Magit | Rebase sequence editing, inspection, undo, and structured commands for basic and topology operations. [Sequence editor manual](https://docs.magit.vc/magit/Editing-Rebase-Sequences.html) | Rich contextual action vocabulary and persistent progress. | Adapt the model to GitUI's command shelf; avoid assuming Emacs key fluency. |
| Git Interactive Rebase Tool | Rust terminal editor with visual multi-selection, actions, reorder, diff inspection, breaks, exec editing, and external editor support. [Project documentation](https://github.com/MitMaro/git-interactive-rebase-tool) | A dedicated plan-editing mode can be powerful and compact. | Extend beyond the todo editor into execution and recovery. |
| GitKraken Desktop | Visual interactive-rebase entry points and per-row actions; documented restrictions for some source histories. [Interactive rebase documentation](https://help.gitkraken.com/gitkraken-desktop/interactive-rebase/) | Clear plan review with a deliberate Start boundary. | Avoid inferring that a polished visual editor implies support for every Git topology. |
| Fork | Interactive editor includes pick, edit, reword, squash, fixup, and drop. [Official introduction](https://git-fork.com/blog/posts/forkwin-1.23/) | Keep common intents close to commit selection; make action labels visible. | Historical announcement establishes the pattern, not a complete current support matrix. |
| Sublime Merge | Reorder/squash/drop and content-edit/split workflows. [Official announcement](https://www.sublimemerge.com/blog/sublime-merge-build-1092) | Connect history editing to existing staging mechanics. | Splitting needs an explicit remaining-change view and recovery context. |
| Tower | Contextual operations, drag reorder, distinct squash/fixup, and editing through Working Copy. [Interactive rebase guide](https://www.git-tower.com/help/guides/commit-history/interactive-rebase/mac) | Manipulate meaningful relationships and reuse a familiar working-copy view. | Translate drag gestures into keyboard placement/target-selection actions. |
| SmartGit | Dedicated plan editor, merge-flattening warning, split via index, and continue/step workflows. [Manual](https://docs.syntevo.com/SmartGit/Latest/Manual/GUI/Branch/Rebase-Interactive) | Strong separation of planning and execution; visible topology policy. | GitUI's Step must have a proven native mechanism before it is exposed. |
| JetBrains | Reword, combine, drop, and reorder in a rebase dialog. [UI walkthrough](https://www.jetbrains.com/guide/java/tutorials/git-interactive-rebase/ui/) | Discoverable editing controls with commit details. | Keep runtime conflicts in the same persistent GitUI frame. |
| GitLens | An interactive rebase editor embedded in the code workflow. [Feature documentation](https://help.gitkraken.com/gitlens/gitlens-features/#interactive-rebase-editor) | Treat rebase as an ongoing workspace integrated with file editing. | Re-observation and correct repository/window identity matter as much as editing controls. |

The Luna research also identified an instructive implementation comparison: JetBrains describes native Git integration and optimization/fallback tradeoffs in its [rebase implementation article](https://blog.jetbrains.com/platform/2026/04/speeding-up-interactive-rebase-in-jetbrains-ides/). This supports investigating native execution, but does not establish that the proposed GitUI adapter or its recovery guarantees already exist.

Research limits: this was documentation/source research, not a hands-on usability benchmark of every current binary. Historical announcements are identified as such; keyboard defaults and product capabilities change. The novel visual mechanisms, Rust boundaries, migration sequence, and acceptance gates are recommendations for this checkout.
