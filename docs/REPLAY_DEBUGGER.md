# Replay debugger tutorial

`asb-replay-debug` is a separate Rust binary using Ratatui and Crossterm. It
opens retained replay mismatch evidence offline. It does not start an agent,
provider, container, or replay. Normalizer management changes project files only
when requested; cassette and mismatch evidence remain unchanged.
An initialized ASB project is not required to inspect an individual file.

## 1. Build the viewer

From the ASB source checkout:

```sh
cd /path/to/asb
make build
make install
asb-replay-debug --help
```

`make build` builds both binaries and `make install` places them under
`~/.local/bin`. Ensure that directory is on your `PATH`. Ordinary root
`cargo build` and `cargo run` continue to select `asb-cli`; use
`cargo build --locked -p asb-replay-debug` to build the companion alone.

## 2. Open your latest campaign

```sh
asb-replay-debug \
  --latest /path/to/project
```

`--latest` chooses the campaign directory with the newest modification time
under `PROJECT/results/asb-executions`, using its path to break ties. It requires
a saved `plan.json`. If that campaign has no retained mismatch, the viewer
reports that fact; it does not silently open an older failed run. For a stable
comparison, select an explicit campaign instead.

An individual campaign can be opened directly:

```sh
asb-replay-debug \
  /path/to/project/results/asb-executions/<campaign-id>
```

A `batch-1` directory or a single `.mismatch.json` file is also accepted.
Campaign discovery examines immediate `batch-*` directories. Opening a single
file still reads sibling run logs when available to identify its replica.
It never follows the absolute paths inside those logs to read other artifacts.

## 3. Inspect the mismatch

The upper list shows the retained cases and their reported execution and
official grading outcomes. Use `[` and `]` to switch cases. Attribution comes
from the run log's reference to the mismatch filename; the scratch suffix is
not treated as a replica number. Missing or ambiguous attribution stays unknown.

The left pane lists differing and redacted fields. The right panes show the
selected field's expected and actual values, with line numbers and value types.
The viewer initially selects a differing field and folds unchanged text,
retaining three context lines on either side of changes.

For example, a selected field may be `$.body.input[50].output`:

```diff
- Ran 6 tests in 0.001s
+ Ran 6 tests in 0.002s
```

The changed `1` is red on the expected side, and `2` is green on the actual
side. Matching characters in the changed line remain unhighlighted. Unchanged
context is gray. A `?` marks a redacted or omitted value, whose original
equality cannot be established. For example, the existing capture redacts
`max_output_tokens`, so that field is shown as unknown rather than equal.

| Key | Action |
| --- | --- |
| `[` / `]` | Previous / next retained mismatch case |
| `Tab` | Switch focus between field list and text panes |
| Up/Down or `j`/`k` | Select a field, or scroll text in text focus |
| PageUp/PageDown | Scroll both text panes together |
| Home/End | First/last field or text page, depending on focus |
| Left/Right or `h`/`l` | Scroll text horizontally |
| `n` / `N` | Next / previous differing field |
| `a` | Show/hide matching fields |
| `c` | Expand/fold unchanged text context |
| `/` | Filter field paths; Enter accepts, Escape clears |
| `r` | Reload the same input from disk |
| `m` | Manage project replay normalizers; create a suggestion from the selected difference |
| `?` or F1 | Show help; any key closes it |
| `q`, Escape, or Ctrl-C | Quit and restore the terminal |

Use a terminal at least 64 columns by 20 rows; 120 columns or more is helpful
for side-by-side output. Lines do not wrap in the diff panes. Use horizontal
scrolling to inspect long lines.

## 4. Start with a specific replica

```sh
asb-replay-debug \
  --latest /path/to/project --replica 3
```

This selects the first retained mismatch attributed to replica 3. When a
campaign contains several batches, open a batch directory to disambiguate.
If the saved run logs cannot attribute the requested replica, selection fails
with an explanation instead of guessing from the filename.

## 5. Print a diff without the TUI

```sh
asb-replay-debug \
  --latest /path/to/project --replica 1 --print
```

`--print` prints the selected case, its known outcomes, differing/redacted
fields, and a line diff. It works with redirected output and no terminal.
`--all` also includes matching fields and all available text context:

```sh
asb-replay-debug \
  --latest /path/to/project --replica 1 --all --print
```

The same `--all` flag starts the TUI with matching fields and full context
visible. Opening a valid diagnostic successfully returns exit status 0 even
when the inspected replay failed; unreadable or invalid inputs return nonzero.

## Browse a recording cassette

Use the same debugger to inspect a multi-track cassette bundle or a legacy
JSONL tape without starting replay:

```sh
asb-replay-debug --cassette /path/to/recording.asb.zip
asb-replay-debug --cassette /path/to/recording.asb.zip --track 2 --print
asb-replay-debug --cassette /path/to/recording.asb.zip --track 1 --native --print
asb-replay-debug --cassette /path/to/recording.asb.zip --track 1 --artifact local-tools --print
```

Tab switches focus between tracks and recorded interactions. Use Up/Down or
`j`/`k` to select, `s` to cycle through provider, native and named artifact
streams, PageUp/PageDown to scroll the selected entry, and `q` to exit. The
selected JSON entry wraps by default; `w` toggles wrapping, and Left/Right
scroll horizontally when wrapping is off. Nonempty string values of `stderr`
are highlighted in red, including nested fields. A
legacy tape has unknown completion and grading status. Incomplete native
sidecars cannot be read by the shared cassette reader and are reported as an
error rather than assumed complete. The
viewer uses ASB's existing cassette source reader and checks a selected
track's recorded length and digest. Selected tracks are limited to 32 MiB
and 50,000 JSONL interactions; a single interactive preview is capped at
200,000 characters. `--print` writes the complete selected text stream.
Non-UTF-8 artifacts show a hexadecimal preview of at most 65,536 bytes in
both modes, with an explicit truncation flag; they are not exported as files.

Cassette contents may contain prompts, tool output and other sensitive data.
The browser shows the recorded contents as stored; protect terminal history
and redirected output accordingly. It reads only the selected track and does
not execute agents, providers, tools or replay.

## Manage normalizers

Press `m` while viewing a mismatch or cassette. ASB finds the containing
project, or use `--project /path/to/project` to select it. The manager lists
profiles and their enabled state. Use Space to toggle, `e` to edit the module
with `$VISUAL` or `$EDITOR`, `E` to edit project settings, `a` to add a blank
starter, and `n` to draft one from the selected difference or tool result.
New modules are disabled in the selected replay policy until reviewed and
enabled. Replay mismatch exploration can still execute unselected modules in
the project `normalizers` directory; see [exploratory mismatch recovery](REPLAY_NORMALIZERS.md#exploratory-mismatch-recovery).
Suggestions match the
observed complete text exactly; refine them for other cases as needed.
The workload is inferred from a campaign plan or bundled cassette when
available; `w` sets it otherwise. You can also open the manager directly:

```sh
asb-replay-debug --normalizers /path/to/project
```

Edits affect future replays. New raw-request recordings permit choosing a
different policy afterward; older normalized recordings retain their original
policy requirement. Editing a module never executes it.

## What this version can establish

The viewer compares all retained request fields rather than stopping at the
runtime's first reported difference. JSON bodies stored as encoded strings
are decoded before comparison. Objects are compared by key, arrays by position,
and missing fields are distinguished from JSON null. Strings receive line and
character diffs. Control characters are escaped for terminal display.

It does not rerun request normalization or claim to recreate the exact final
runtime comparison key. The evidence is already normalized/redacted. The
original inputs, normalization-rule history, complete replay request timeline,
and bodies of later retries are not available in these mismatch files. Only
the first retained mismatch per proxy is shown. An absent file does not prove
that a replay matched.

Each input file is limited to 32 MiB, directory scans and total mismatch cases
to 4096, and each structural comparison to 50,000 leaf fields. Only the selected
case's request bodies are loaded for comparison. Text previews show at most
20,000 lines per side and 8192 characters per line. Large comparisons fall back
to positional line alignment or common-prefix/suffix character ranges, with
an explicit notice. Field equality is computed before these preview limits;
changes outside a clipped preview remain reported as field differences.
Run logs use their last nonempty record; malformed final records produce a
warning and never borrow attribution from an earlier attempt.
