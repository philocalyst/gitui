# Selection-first modal workflow

This design note records the interaction contract for the Helix-inspired GitUI
profile and the safety boundaries around its branch and rebase workflows. The
classic profile remains available with `gitui --keymap legacy`; `KEY_CONFIG.md`
documents how custom RON files patch whichever profile is active.

## Interaction contract

Helix starts in a normal navigation state. `h`, `j`, `k`, and `l` move the
current cursor; arrow keys remain usable, and a numeric count applies to the
next movement, capped at 999. For pane focus, press `Ctrl+w` and then `h`, `j`,
`k`, or `l` (or an arrow key); `w` cycles panes. Esc cancels, and an unknown
second key is consumed. Tab/BackTab also cycles available panes. Direct
`Ctrl+h/j/k/l` keys move focus too; the prefix avoids relying on
terminal-specific key remaps. The `g` prefix opens a tab or location, Space
opens common actions, and `:` opens a fuzzy command palette. A prefix stays
active until its second key or Esc, so an unknown second key cannot
accidentally run an application command. Search and palette text entry
consume their own printable keys.

| Key | Action |
| --- | --- |
| `g 1` through `g 7` | Status, Log, Files, Stashing, Stashes, Rebase, Branches workspace |
| `g b` | Open the quick branch picker |
| `Ctrl+w` then `h/j/k/l` or arrows; `w` | Move focus by direction; cycle panes |
| Tab / BackTab | Cycle pane focus |
| `Space b` / `Space B` | Open the Branches workspace / quick branch picker |
| `Space c` / `Space r` | Open the commit editor / interactive rebase |
| `c` | Open the configured Git commit editor from any Status pane |
| `:` | Search available commands |
| `v`, `x`, `%`, Esc | Enter/extend selection, toggle an item, select all, clear selection |
| `s` / `u` | Stage selected files or diff changes / unstage them in the staged pane |
| Enter | Inspect Status; toggle a diff fold; stage selected lines in working-tree Patch |
| `Shift+C` | Open the inline commit editor from any Status pane |
| `q` / Esc | Quit GitUI / cancel the active mode or close the current overlay |
| `Shift+A` | Stage or unstage all files in the focused status pane |
| `Ctrl+Z` | Open the commit undo flow from the log |
| `x`, `/`, `Ctrl+G` | Mark a log commit, search, toggle graph display |
| `Ctrl+S`, `Alt+K`, `Ctrl+X` | Skip a stopped rebase step, keep an empty step, insert an exec step |

`v` begins selection or range extension, `x` toggles the current item, `%`
selects all items supported by the focused pane, and Esc clears the selection.
In the Files pane, `%` includes files beneath collapsed directories; in the
commit log, it selects the loaded commit window. The selection travels with
the current view: commits can be marked as an explicit set or range, diff
lines and hunks can be selected, and working-tree files can be selected for
batch stage/unstage. Read-only history and branch-comparison diffs keep these
staging actions disabled. The relevant commands expose selection counts so the user
can see their scope before acting. For example, `v`, `2j`, `s` selects and
stages a three-file range in the Files pane. In a diff, `x` marks the current
hunk and `s` stages its selected lines; `u` unstages selected lines when the
index diff is focused. In either Patch or Review, Enter on a fold summary or
region header expands or collapses it. In working-tree Patch, Enter on a
selected diff line stages that selection. Enter in Status inspects the selected
file or moves focus into its detail pane. Multi-commit operations are copy and
compare; the rebase plan mutates one step at a time.

In the commit log, exactly one marked commit remains the target for checkout,
tag, reset, revert, reword, and rewrite actions even after the cursor moves.
With multiple commits marked, those single-commit actions are disabled or show
a clear-selection message; copy and compare still use the marked commits. In
the stash list, apply, pop, and drop require cleared marks. A commit selection
does not imply a batch stash action.

The command palette gathers available commands from the focused application
context, displays each command's active binding, and searches labels,
descriptions, and groups. Arrow keys or the mouse wheel move through results;
a mouse click selects a row, and Enter runs it. Clicking never runs a command.
A duplicate executable binding is shown as a conflict and cannot be run from
the palette. The Help view and contextual command bar use the same binding
metadata.

Help and Options are overlays whose arrow keys navigate entries and settings.
In the fuzzy picker, arrows navigate results while `j`, `k`, `g`, and `q`
remain literal search text.

## Branch picker

The Branches workspace is a dedicated tab (`g 7` or `Space b`); `b` selects a
comparison base, `c` checks out the selected branch, `n` creates a branch, and
Enter inspects the selected branch or commit. The quick branch picker remains available with `g b` or
`Space B`; it filters local or remote branches as the user types. Its Space
action layer holds branch operations, so text that happens to be `d`, `r`, or
`m` only narrows the result
list. Enter checks out the selected branch; Space then `c/r/d/m/e/x` creates,
renames, deletes, merges, rebases, or compares. The current branch cannot be
deleted or merged/rebased into itself. Esc first cancels the action layer,
then clears the filter, then closes the picker. The tab key switches
local/remote scope.

The comparison graph shows commits from the selected branch and its base, with
their actual parent links and branch roles. `Ctrl+G` switches between that graph
and the selected branch's contribution. `+` loads the next 200 commits without
changing the selected commit; End moves to the last loaded row.

## Interactive rebase

The existing interactive-rebase workflow remains a reviewed plan. The user
can reorder, reword, edit, squash, fix up, drop, break, or add an exec step;
plan edits support undo/redo. A rail shows the ordered plan, the inspector
shows commit details, and conflict stops provide continue, skip, keep-empty,
and abort actions. Starting the plan is a separate explicit action. Rebase
step selection is intentionally distinct from execution: `i` toggles the
inspector, `Ctrl-S` skips the stopped step, `Alt-K` keeps an empty commit, and
`Ctrl-X` inserts an exec step in the rebase view.

## Keymap and collision rules

RON entries override only the keys supplied, leaving other values from the
active profile in place. The Helix profile checks that movement, focus,
selection, prefix, and goto tab keys do not shadow one another. It also rejects
an action binding when that binding is intercepted by a modal control.
Contextual commands may share a key when they belong to different views; the
command palette reports ambiguous bindings rather than guessing.
Terminal events that report Shift both in the character and as a modifier are
matched consistently. Hints show all configured Ctrl, Alt, Shift, and Super
modifiers instead of dropping combinations.

## Inspiration and prior discussion

The navigation and prefix design borrows from Helix's normal, window, goto,
Space, search, and select/extend modes, adapted to GitUI's focused panels and
Git commands. [Helix window mode](https://docs.helix-editor.com/keymap.html#window-mode)

GitUI users have discussed accidental collisions in the Vim-style profile,
pane focus, and distinct stage/unstage-all actions. These issues are useful
context for keeping navigation separate from focus and making action scope
visible: [key collision](https://github.com/gitui-org/gitui/issues/2704),
[pane focus](https://github.com/gitui-org/gitui/issues/1955), and
[stage/unstage all](https://github.com/gitui-org/gitui/issues/2352).

[Guitar](https://github.com/asinglebit/guitar) is another terminal Git client
whose graph-first navigation and explicit action mode informed the emphasis
on context, scope, and gating sharp operations.

## Embedded diff review

Read-only commit and branch inspections show native Review panes inline, and
the Branches workspace starts there by default. The Status worktree starts in
Patch view so selection and staging keep using exact Git diff rows. `Ctrl+T`
switches Review/Patch where both views are available; Review rows never map to
staging positions. Revlog keeps a commit inspector beside the graph, showing
metadata, the selected commit's files, and a read-only native diff while the
selection moves. Review renders the selected native diff. The Options panel
chooses Inline suggestions or Plain unified rows over the same source diff and
hunk context. Inline suggestions uses grapheme-precise alignment for
code-like lines and whole-word alignment for prose. Markdown and plain-prose
rewrites can reflow several source lines together at the pane width while
retaining their original patch-row mapping; fenced code remains line-oriented.
Aligned old/new number gutters identify source lines, and replacement rows
have no extra change glyphs. Syntax foregrounds remain visible with inline
added/removed emphasis. The Review title names the active presentation as
Inline suggestions or Plain unified; binary and other special diff statuses
keep their status label. Where supported, Tree-sitter analysis recognizes a
function or test declaration with multiple represented patch rows, and Inline
suggestions can fold those rows. A titled fold summary shows its represented
patch-row count and exact change counts; expanding restores the original rows.
It does not reconstruct source lines omitted from the diff. Plain unified
leaves patch rows separate and unfolded. Historical per-repository Difftastic
and Delta setting values load as Inline suggestions without changing other
saved options. In working-tree Patch, selecting a paired replacement selects
both underlying lines and stages them atomically while preserving intervening
sibling rows. Plain unified keeps raw lines independently selectable. Review
remains read-only. The Status and Branches Patch views keep exact Git rows for
staging actions. The
focused Status or Branches diff can be expanded with `z` and restored with Esc.
Leaving a zoomed diff through pane focus also restores the split layout first.

The commit log starts with the visible rows and a bounded lookahead, then
requests more history as the selection approaches the loaded end. Its title
shows the loaded count, a bound key for older commits, and a loading marker
while another range is arriving. Commit IDs and metadata are cached
separately, so graph rows and the selected commit inspector fill in as those
results arrive. Searching the full history is an explicit operation: it
walks the remaining history and Esc cancels that walk. The file history
inspector follows the same visible-range loading model. In the Branches
workspace, `+` loads 200 more commits without moving the selected row; `Ctrl+G`
switches between the full comparison graph and the selected branch's
contribution.
