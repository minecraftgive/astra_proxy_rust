# TUI interaction and resident-host status

The six main pages share a region focus model. `Tab` / `Shift-Tab` change regions;
arrows select within the region and `Enter` activates the focused item. The active
configuration is independent of the row being browsed in Profiles.

| Key | Action |
| --- | --- |
| Ctrl-K / F2 | Command palette |
| F1 | Keyboard help |
| Ctrl-P | Choose the current configuration |
| 1–6 | Change pages outside text input |
| Ctrl-F, F3 / Shift-F3 | Search, next / previous match |
| Ctrl-S, Ctrl-Z / Ctrl-Y | Save and apply, undo / redo |
| Space | Pause the connection/log display, or toggle a focused control |
| F6, F7, F8 | Tasks and update status, TUI log, new window |
| Esc | Leave one layer, retaining configuration drafts |
| Ctrl-C, q in browse mode | Save the session and close this interface |

Inside the Ctrl-P configuration chooser, Tab cycles through search, configuration
type, and results. Arrows on the type control choose Proxy or Rules; Enter moves
to the results, where Enter selects a configuration. Choosing a rule core opens
that object in Profiles without changing the active proxy profile. Page Up / Down
in either palette moves by the visible result count.

Restart, stop all, external editing, import/export, and explicit draft discard are
named commands. Destructive confirmations start on Cancel. Legacy `Q`, `R` and
`0` no longer execute destructive actions. A migrated session retains its draft
and shows a dismissible keyboard notice. Source files still use optimistic
concurrency checks: an external edit is never silently overwritten.

In the node editor, `v` selects the current node and `V` selects all or clears
the selection. `Ctrl-E` exports selected nodes as TOML; `Ctrl-Shift-E` exports
share links. Both offer **File or folder** and **Clipboard**, also available in
the F2 command palette. Node file exports default to the current user's Documents
folder (including redirected Windows Documents folders); the path can be edited.
Relative export paths are resolved from that folder. File exports create missing parent directories and keep
existing files intact. A directory (or a path without a filename extension) uses
`nodes.toml` or `nodes.txt`; supply a full filename to choose another name.

`Ctrl-I` accepts node files, pasted TOML/YAML, or multiple share links. Multiple
file paths can be pasted together, quoting paths that contain spaces. Review the
node list before importing: Space selects a row, `a` selects all or clears, Enter
imports, and Esc cancels. Only selected nodes are appended to the draft; save
with `Ctrl-S`. Imports remain undoable, and export selection clears after edits
so it cannot silently point at changed nodes.

For a stopped Proxy or Rules profile whose previous session cannot be recovered,
choose **Force start** from its operations and confirm the replacement. This
discards old session markers and identity records for the profile and stopped
startup dependencies; managed running dependencies remain running. Normal Start
retains its safety checks. Force start never kills a PID from an unverified
record, and an active or inaccessible OS endpoint may still prevent replacement.

Pause freezes a view, not the collectors. Logs continue to receive and apply
rotation; runtime health and update outcomes continue to arrive. Resume shows the
latest state without replaying intermediate frames. A missing connection remains
an unavailable selection instead of selecting its neighbor. The bounded runtime
list is a sample, not a complete list of active connections.

## Updates and data freshness

Import validates and stages a local release. Online activation is a separate
experimental action, available only when the resident host and release satisfy
the existing gates. The confirmation fixes its target before submission.

F6 distinguishes the staged release, new-flow release, draining generations,
persisted next-launch selection, and resident host versions. Worker activation
does not replace a resident host or migrate existing connections. Host maintenance
requires a restart and does not guarantee FakeIP mapping persistence.

Each new update request has a durable operation ID. Repeating that ID observes
the same operation. After an unknown or partial result, refresh status and recover
that operation; recovery repairs recorded remaining steps without activating a
second generation. Status subscription is read-only and remains available while
the mutation actor works.

Enhanced runtime data carries the network host's boot identity, active generation,
sample sequence and source time. Repeated cached samples do not add zero-speed
points. Partial or old data is explicitly marked; a worker generation switch
does not reset totals. Older hosts remain usable with source freshness marked as
unverified.

## Settings information

Settings opens with read-only program information above the preferences. The
local TUI build and system are labeled separately from the selected management
node. Build timestamps are UTC; platform support describes implementation support,
while TUN, system proxy and LAN indicators reflect fresh runtime samples. Missing
or stale runtime data is shown as unknown. A failed metadata refresh retains the
previous information with a cache warning.

Use `Page Up` / `Page Down` or the mouse wheel to scroll the information and
preferences, `Home` to return to the top, and `r` to refresh node information.
Arrow keys still select and change preferences, keeping the selected row visible.

## Motion

The command palette offers **Motion: normal**, **Motion: reduced**, and
**Motion: off**, persisted per interface session. Normal uses interruptible focus
and page indicators, row reveals for overlays, and real-sample chart emphasis.
Input takes effect on the first frame. Reduced and Off use immediate geometry
and static waiting indicators.

Normal is capped at 30 fps locally and 15 fps when `SSH_CONNECTION` or `SSH_TTY`
is present. Core sampling and IPC requests are independent of drawing. Idle
frames are not rendered, and `NO_COLOR` disables color without changing motion.
External editors and terminal elevation receive terminal input after the TUI
releases its event stream.
