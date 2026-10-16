# Interaction review

This review records the concrete input, Git, and startup failures found during
the workflow changes and the repairs made for them.

## Input and selection

The App once consumed a content click while checking whether it hit a tab. The
router now sends it to the active pane. Trees and commit lists record hit rows
while drawing, and a drag remains with the pane that received mouse-down, so a
click or drag acts on the row shown under the pointer. A 16,384-row status
fixture also caught redraw work that traversed every row; rendering now formats
only the visible viewport.

Direct `Ctrl+h/j/k/l` pane focus remains available. Helix also has a portable
`Ctrl+w` prefix followed by a direction key; its `w` chord and Tab/BackTab cycle
panes. The prefix avoids relying on terminal-specific key remaps; Esc cancels
and unknown chords are consumed.

Terminals with enhanced key reporting distinguish repeats and releases from
initial presses. GitUI can then suppress repeated mutating commands while
keeping repeat text for active text capture; terminals without those event types
retain legacy press handling. Selected diff lines can also outlive the contents
they describe: before staging or resetting, GitUI checks that the displayed
selection still matches the current diff and rejects stale content. Tree actions
use literal path scopes; folder selection covers that folder's descendants, an
empty selection cannot become a whole-repository reset, and symlink paths do
not escape the worktree.

## Editor and repository updates

Returning from the external editor called `Terminal::clear`, whose cursor
position query could race with resumed input and terminate GitUI. The return
path now resets the full screen and resize state without querying the terminal.
An editor-return regression covers the query-free path. The editor runs in
cooked mode; after it exits, GitUI restores its raw mode, alternate screen, and
mouse state. On failure, GitUI retains the edited text. In the Helix profile,
`c` starts the configured editor from any Status pane; `Shift+C` opens the
inline form. Terminal checks cover stage/unstage flow, nonzero editor exit,
message recovery, and terminal-state restoration.

Removing a repository during an asynchronous log read could panic; the log
reader now reports the failure and retries. Async diff had two separate
failure modes: a closed notification receiver could panic, and a missing-repo
request's failure notification could retrigger the request in a retry storm.
Both paths now stop cleanly. Startup uses two phases so the interface can
appear while repository data loads. Workers reuse validated observer
configuration instead of repeating startup probes.

A crash during rebase bootstrap can leave the recorded original branch name
missing. Explicit Abort repairs that name only when the saved original tip
still matches `HEAD`, the index and worktree are clean, and no replay progress,
autostash, or unrecognized metadata exists. Git then performs the abort. If any
evidence is missing or unsafe, recovery leaves the repository untouched.

In an earlier three-run source-and-binary benchmark, with warmed
caches, the 16,000-file/30,000-commit repository, a 120×30 PTY, `NO_COLOR`, an
empty key config, and the same ticker, median time to the first file was 0.24 s
at baseline `69b1e5c` and 0.08 s in the candidate. Median first-frame time was
0.12 s and 0.06 s, respectively. The candidate showed `[LOADING]` with files
in all three runs; the baseline did not. One candidate run was slower than its
paired baseline (0.76 s versus 0.40 s), so load and cache state affect results.
This warm-cache debug measurement describes one fixture and an earlier
checkpoint, not the current source, release builds, or all repositories.

## Branch history

A branch comparison drawn as a single-parent chain hid merge structure. The
Branches workspace now builds a bounded graph from the selected branch and
comparison base, keeps actual parent links, and labels each branch's role. The
default view shows their union; `Ctrl+G` shows only the selected branch's
contribution. `+` loads the next 200 commits and keeps the selected commit ID;
End moves to the last loaded row.

A five-second refresh could erase a good graph before a six-second comparison
finished. Refresh now reads local refs and `HEAD` asynchronously and applies
local and remote state together. Unchanged refs retain the current graph;
branch actions wait for the refs check, then a changed comparison reloads. This
also detects detached-`HEAD` changes. Topology and Contribution views keep
independent cursors, even when one side has no contribution. Stale results are
discarded when the branch or base changes. Selection highlighting spans the
full inner row in Branches, History, and Files. When an external ref moves, the
previous diff clears immediately; a late result for the old ref cannot restore
it.

A graph-cache regression once left the rendered-entry cache at its initial 128
rows after the commit-ID stream grew to about 18,000, so selecting row 138
could render blank. That historical implementation cached a growing full-log
window. The current log instead loads bounded pages around the visible range;
the graph cache validates the active page's offset, length, and commit IDs,
then clamps the viewport when the loaded range or terminal size changes.

Merge rows also placed a second parent lane at the far right, widening the
graph unnecessarily and squeezing out the commit message. The second parent is
now routed beside the merge where the existing lanes allow it. In the `b40`
fixture, both parent connections remain across 13 live lanes and the following
one-lane fork stays in its column. Narrow views cap the graph gutter and mark
clipped continuations with an ellipsis; a front-view `M` may still bridge other
existing lanes, so the width reduction is not claimed for every viewport.

## Embedded review follow-up

Read-only previews use typed native diff rows and never invoke Difftastic,
Delta, or another external renderer. The source cache is keyed by repository
identity, diff parameters, validated captured-source hash, and preview
preference; terminal width is excluded because source diff rows do not depend
on it. Resizing reflows prose blocks and updates the viewport without rerunning
the diff. Cached syntax styles are reused, with background completion
notifying the UI to redraw.
Inline suggestions aligns text-similar deleted and added lines
into a typed row with separate old/new line numbers. Code-like lines use
grapheme-precise alignment; prose uses bounded whole-word alignment. The
backend classifies prose from captured source, including Markdown fences, so
fenced code remains line-oriented. A prose display block can represent
several old and new patch rows; shared words appear once and new-side
paragraph breaks are preserved. It reflows to the pane width while original
line and patch-row identity stays attached. The aligned old/new gutters carry
the line numbers without redundant change glyphs. Syntax foregrounds remain
visible on ordinary and cursor rows and under inline emphasis; deleted spans
use a muted red background and strikethrough, while added spans use a muted
green background and underline. In working-tree Patch, selecting a paired
replacement stages its underlying lines atomically while preserving
intervening sibling rows. Plain unified leaves raw lines
independently selectable; Review rows remain read-only.
Inline suggestions also applies background Tree-sitter analysis on supported
source types (Rust, Zig, TypeScript/TSX, JavaScript/JSX, Python, and JSON).
Fold controls are limited to recognized function, method, or test regions and
appear only when multiple represented patch rows belong to the region. A fold
collapses only contiguous patch rows already present in the preview, with
hunk headers between them. Its titled summary reports the represented
patch-row count and exact added and removed counts; expanding restores those
original rows. The preview does not
invent source text or claim to hide context omitted by Git. Unsupported,
malformed, or over-budget syntax analysis leaves the ordinary rows visible.
Plain unified keeps the original separate patch rows without syntax folding.
When a source changes, a native diff is pending, a view is hidden, or capture
fails, GitUI cancels the prior job and clears the content and path label; stale
completions cannot restore them. Wheel and configured keyboard navigation stay
within the preview pane. In Status, `Ctrl+T` switches Patch/Review; Branches and
read-only history default to Review. `z` expands Status and Branches diffs, and
Esc or moving pane focus restores the split layout. Patch view retains exact
native Git rows for staging and selection.

Accepted preview results carry a generation and exact source identity so late
work cannot replace a newer diff. Wrapped prose projections are rebuilt for
the viewport width, with a source-backed scroll anchor preserved on resize.
Projection and fold grouping are cached instead of rescanning all preview rows
on every draw. Highlight work is bounded and prioritizes visible rows before
context; syntax results arrive asynchronously and only trigger redraws.

Partial staging also exposed a Git index timestamp edge case: an unrelated
same-size edit made in the same timestamp second could otherwise be mistaken
for unchanged content. Before a partial index write, GitUI invalidates
second-resolution stat data only for stage-0 entries in that same-second race.
A regression verifies the unrelated edit remains unstaged and that older
entries and conflict stages are preserved.

The design discussion draws on the [Diffr article](https://dev.fast/blog/diffr)
and its [primary source tree](https://github.com/devdotfast/whiteboard/tree/main/diffr),
along with Delta's [two-column line-number gutter](https://dandavison.github.io/delta/line-numbers.html).
GitUI implements its own bounded in-process syntax-region folding and
text-level replacement alignment. It does not include Diffr's Difftastic
engine, plugin execution, or generated pseudocode, and does not run those tools
as subprocesses.

## Verification

The final run passed 358 UI tests (1 ignored) and 447 backend tests (1 ignored;
2 signing tests excluded because OpenPGP/X.509 signing setup was unavailable).
All 29 integration tests passed: 14 preview, 6 paired-staging, and 9
structural. One isolated child was ignored by the parent suite but invoked
explicitly. The two supporting test groups passed 31 file-tree and 31 git2
hook tests, plus 1 asyncgit doctest. Strict all-features workspace Clippy,
formatting, and diff checks passed.

All 44 terminal checks passed: 8 prose, 3 GPUI including MainLog, 6 suggestion,
5 folding, 8 scrolling, 10 pointer/editor, 3 large-runtime, and 1 lazy-history
check. On the 40,000-commit fixture, the initial 68 loaded IDs stayed at 68
after idle time and reopen; explicit older-page requests stayed below 1,024
rows. The review capture is at the ignored path
`target/review-artifacts/prose-reflow-review`. Repository HEAD remains
`69b1e5c`; no commit or push was made.
