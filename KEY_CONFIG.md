# Key Config

GitUI starts with the selection-first **Helix** profile. Choose the classic
arrow-key and Vim-style action layout with `gitui --keymap legacy`. The profile
selects both default bindings and the input workflow; it does not rewrite any
configuration file.

You can change either profile with a `key_bindings.ron` file. The file is a
partial patch: omitted keys keep the defaults from the selected profile. For
example, this patch changes navigation while preserving the other Helix keys:

```ron
(
    move_left: Some(( code: Char('h'), modifiers: "")),
    move_right: Some(( code: Char('l'), modifiers: "")),
    move_up: Some(( code: Char('k'), modifiers: "")),
    move_down: Some(( code: Char('j'), modifiers: "")),
    open_help: Some(( code: F(1), modifiers: "")),
)
```

Pass a file explicitly with `--key-bindings path/to/key_bindings.ron`. Otherwise
GitUI looks for it in the platform config directory:

* `$HOME/.config/gitui/key_bindings.ron` (macOS and Linux)
* `$XDG_CONFIG_HOME/gitui/key_bindings.ron` (Linux when XDG is set)
* `%APPDATA%/gitui/key_bindings.ron` (Windows)

The older [Vim-style config](vim_style_key_config.ron) remains available. It is
applied as a patch to the selected profile, so select `--keymap legacy` when you
want that file to retain the classic surrounding workflow.

In the Helix profile, `h j k l` move and the arrow keys remain available;
numeric counts apply to the next movement (`3j` moves down three rows).
Use `Ctrl+w` then `h`, `j`, `k`, or `l` (or an arrow key) to move focus between
panes; `w` cycles panes. Esc cancels the sequence, and an unknown second key is
consumed. Tab/BackTab also cycles panes. Direct `Ctrl+h/j/k/l` keys move focus
too; the prefix avoids relying on terminal-specific key remaps. The main
shortcuts are:

| Keys | Action |
| --- | --- |
| `g 1`–`g 7` | Status, Log, Files, Stashing, Stashes, Rebase, Branches workspace |
| `g b` | Open the quick branch picker |
| `Ctrl+w` then `h/j/k/l` or arrows; `w` | Move focus by direction; cycle panes |
| Tab / BackTab | Cycle pane focus |
| Branches workspace: `b` / `c` / `n` / Enter | Choose a comparison base / check out selected branch / create a branch / inspect selection |
| Branches workspace: `Ctrl+G` / `+` / End | Toggle graph scope / load 200 more commits / select the last loaded row |
| `Space b` / `Space B` | Open the Branches workspace / quick branch picker |
| `Space c` / `Space r` | Open the commit editor / interactive rebase |
| `c` | Open the configured Git commit editor from any Status pane |
| Status / Branches diff: `Ctrl+T` / `z` / Esc | Switch Review/Patch / expand the diff / restore the split layout |
| `v`, `x`, `%`, Esc | Enter or extend selection, toggle an item, select all, clear selection |
| `s` / `u` | Stage the selected files or diff changes / unstage them in the staged pane |
| Enter | Inspect the selected item in Status; in a working-tree Diff, stage selected lines |
| `:` / `/` / `?` | Open the command palette / search the current view / open Help |
| `Shift+C` | Open the inline commit editor from any Status pane |
| `q` / Esc | Quit GitUI / cancel the active mode or close the current overlay |

The command palette searches available actions and shows their active binding.
Use the arrow keys or mouse wheel to move through results, click to select a
row, and press Enter to run it. A click only selects; it never runs a command.
The active profile's Help screen shows the current bindings and selection
scope. Reset, delete, and drop actions use uppercase bindings or an explicit
action prefix so they stay separate from everyday movement and staging.
`branches_create`, `branches_checkout`, `branches_set_base`, and
`branches_load_more` configure actions in the Branches workspace; the existing
`create_branch` and `select_branch` keys continue to control the quick branch
picker.
`window_prefix` configures the portable pane-focus prefix (`Ctrl+w` in Helix;
unset in Legacy). On terminals with key-event disambiguation, the direct
`focus_left/right/up/down` bindings also work.
`diff_view_toggle` and `diff_zoom_toggle` configure the Review/Patch switch and
focused diff zoom keys.

See [the key definitions](src/keys/key_list.rs) for every configurable field.
`code` values are defined by crossterm's
[`KeyCode`](https://docs.rs/crossterm/latest/crossterm/event/enum.KeyCode.html);
`modifiers` values use
[`KeyModifiers`](https://docs.rs/crossterm/latest/crossterm/event/struct.KeyModifiers.html).
In the Helix profile, GitUI reports an error on startup when movement, focus,
selection, prefixes, goto tabs, movement counts, or actions collide with
reserved modal controls. A malformed RON patch also reports a startup error.
Bindings used by different contextual views may intentionally share a key.

# Key Symbols

The display of special keys can also be customized in `key_symbols.ron`.
For example:

```ron
(
    enter: Some("enter"),
    shift: Some("shift-"),
)
```

Pass a symbols file with `--key-symbols path/to/key_symbols.ron`, or place it in
the same config directory as `key_bindings.ron`. See
[`src/keys/symbols.rs`](src/keys/symbols.rs) for every available symbol.
