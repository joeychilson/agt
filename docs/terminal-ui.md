# Terminal UI

The terminal UI lives in `src/tui.rs` and `src/tui/`. It draws fullscreen on the
alternate screen and shows the session through the agent's updates, so a
resumed session is shown by the code that shows a live one.

## The screen

From the top down:

| Part             | What it shows                                                                                                     |
| ---------------- | ----------------------------------------------------------------------------------------------------------------- |
| Header           | agt's version, the working directory and its branch, then the model, effort, provider, context used and cost      |
| Transcript       | The session; a new one shows its skills and MCP servers with a description each, and the keys to start with       |
| Waiting messages | Messages for the agent, with when each goes out: at its next step, or once it is done                             |
| Input box        | The draft, growing to ten rows or a third of the screen                                                           |
| Status line      | What the agent is doing, or the keys to know, and how many processes run in the background, a count a click lists |

Menus open as panels over the transcript, just above the input box, so a draft
stays where it is.

The status line names what the agent is doing in one word, `Thinking…`,
`Writing…`, `Working…` while tools run, `Retrying…` or `Compacting…`, with the
turn's elapsed time. The branch is read from `HEAD` when the session starts and
after each turn. A server's description on a new session is what it said it is
for, else its tools, else, before it is first used, its program or address.

The palette is the terminal's own 16 colors, so light and dark themes both work:
cyan marks the user and what has focus, green and red how calls ended, yellow
what is still going on, and dim text what is secondary.

## Drawing

A frame is a grid of styled cells, compared with the frame the terminal shows.
Only the cells that changed are written, in one synchronized update with the
cursor hidden while they are, at most every 16 ms and every 80 ms while a
spinner turns. Unchanged rows are skipped whole, and the frame and output
buffers are reused, so an idle frame writes nothing and a busy one costs what
changed.

- **Nothing is ever cleared.** After a resize every cell is written again over
  the old ones, so nothing flashes.
- **The terminal's columns always agree with the frame's.** Autowrap is off, so
  writing the last column never moves the cursor. A cell holds a character one
  or two columns wide; characters of width zero, such as combining marks, are
  left out, and control characters are never written.
- **Nothing wakes while idle.** Mouse reports cover buttons, drags and the wheel
  but not plain movement. The input thread uses crossterm's poll backend, which
  keeps pending input when a resize and a key arrive together, and blocks while
  idle.

## Transcript

The transcript is built from the agent's updates as entries. It starts at the
top of its area and follows the newest rows once they fill it. After the user
scrolls up it stays on the entry it shows while more arrives, and a pill above
the input box counts the rows below. Starting or resuming a session starts it
over. It keeps at most 8 MB of text and drops its oldest entries past that; the
session log still holds them.

| Entry      | How it shows                                                                                                                                                |
| ---------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Message    | `❯` and its text, folded to six rows past eight; `[agt send]` after the `❯` for a message `agt send` delivered; attached images as chips with their size    |
| Work       | What the agent does between speaking: reasoning, calls, and notices raised meanwhile, such as retries                                                       |
| Call       | A step of work: `✓`, `✗` or `■` for a call that succeeded, failed or was interrupted, or a spinner while it runs, then its command and what its result adds |
| Reasoning  | A step of work: the heading of the section being written, or `Thinking`, and its last rows; finished, `Thought for 4.2s`                                    |
| Compaction | `✓ Compacted context  182k → 24k tokens`: a step of the work it happened during, or a row of its own, as after `/compact`                                   |
| Reply      | Markdown                                                                                                                                                    |
| Notice     | A dim line starting with `·`; an error is a red one starting with `✗`                                                                                       |

- **Work folds when the agent speaks.** While work goes on, `Working` shows its
  elapsed time and its newest six steps as they happen. When the agent starts a
  reply, or its turn ends, the work folds into one line, such as
  `▸ Worked for 38s · 6 commands · compacted · 1 failed`, which a click opens
  and closes. A message the user sends mid-turn ends the work before it.
- **A call shows what matters.** Its row leaves out a `cd` into the working
  directory and adds a failing status, `interrupted`, a process left running
  with its id, the output's line count, and durations of a second or more. A
  running call shows its newest output lines, and a failure its last five.
  Opened, a call shows its whole command and all the output the model got.
- **Images are chips that open.** The images a call's output showed are chips
  naming the file, the original's size and any region viewed: all of them under
  the call's row while it is closed, and where the output showed them once it
  is opened. A click on a chip, or on a message's image, opens the saved image
  the model saw in the system's viewer.
- **Reasoning collapses.** Finished reasoning opens to its whole text, work that
  only reasoned opens straight to that text, and brief reasoning a model keeps
  hidden is left out.
- **A finished entry keeps its rows** until the width changes or it is opened
  or closed, so a frame costs what changes rather than the session's length.

### Replies

A reply is CommonMark with GitHub's tables, strikethrough and task lists, read
with pulldown-cmark. Its line breaks are kept, as chat interfaces keep them, and
a blank row separates blocks where the source has a blank line. Code shows under
a rule naming its language, quotes behind a bar, and tables in borders. A table
too wide for the terminal shrinks its columns and wraps their text, and one that
cannot shrink enough shows a record for each row. A click on a link, or on a web
address in text, opens it and names it in the status line.

A reply streams in chunks. A chunk ends before a line that starts in the first
column after a blank line, outside a code fence, since no later line changes how
the lines before it read. A finished chunk keeps its rows, and only the last
chunk is laid out again when text arrives. Until the reply is complete, a line
that is only a marker so far, or a table's row, waits for the rest of it, and
open code, bold, strikethrough and link addresses are closed, so no marker
flashes. A partial response that a retry discards is removed.

### Scrolling and copying

The wheel and Page Up and Page Down scroll. Ctrl-O browses from the keyboard: Up
and Down select entries, Shift-Up and Shift-Down the user's messages, Enter
opens, and Esc or a typed character returns to the input.

Dragging selects text, shown in reverse video, and letting go copies it through
the terminal (OSC 52); a click that does not drag opens what it lands on, and
dragging past the transcript's ends scrolls it. Laid-out rows record the layout
before their text and whether a wrap broke them, so a copy leaves out margins,
gutters, marks and rules and joins what a wrap broke: a long paragraph or
command copies as one line, and a copied table is still Markdown. A selection
holds its place in the text as more arrives; a key, a click or a new width
clears it.

## Input

- **The box follows the cursor.** Vertical movement uses display columns and
  continues into the input history at the edges. A very small terminal shows
  only the current input row.
- **Large pastes and images are chips.** A paste over 10 lines or 2000
  characters becomes a chip, as does an attached image: a label that moves and
  deletes as one unit. A pasted text chip expands when the message is sent.
- **History is bounded.** It keeps at most 100 entries and 256 KB, without
  images; a larger message is sent intact but left out of history. Browsing
  history keeps the draft.
- **Images attach from pastes and the clipboard.** A paste that names only
  existing image files, as a terminal pastes a dropped file, attaches them.
  Ctrl-V attaches the image on the clipboard, read with `osascript` on macOS and
  `wl-paste` or `xclip` elsewhere. When the message is sent, its images are
  prepared as `agt view` prepares images, on a thread, and its text and images
  reach the model in the order they were written.
- **`@` mentions files.** It lists the working directory's files: those
  `git ls-files` names in a repository, so ignored files stay out, or a bounded
  walk elsewhere, listed on a thread the first time. Choosing one puts its path
  in the message.

## Menus

- **One list serves every menu and completion**: commands and skills after a
  leading `/`, files after `@`, then providers, sign-in methods, models,
  efforts, sessions, processes, MCP servers and keys. Typing filters it, ranking
  prefixes, then substrings, then letters in order.
- **A list is a table.** Each column is as wide as its widest cell, measured
  when the items are set, so rows line up however the list scrolls or filters.
  Where the width runs out a description is cut first, then labels down to half
  the row, then value columns are left out from the right. A long list costs
  only the rows it shows. The selected row is a bar in reverse video, and a click
  picks a row.
- **Commands run from the list.** After `/`, commands and skills are listed
  under their headings until a name is typed, and match by name without the
  slash. Enter runs a listed command the input is a prefix of, or a skill it
  names; a loose match is only completed. A completion leaves the cursor in the
  input, and a menu with a search row takes it there.
- **Setup saves as it goes.** It asks for the provider, how to sign in (a saved
  credential, the browser or a pasted key), the model and the effort, and saves
  each choice as it is made; while signing in, Esc goes back to the providers.
  Keys are typed hidden. A browser sign-in shows its address, which a click
  copies. Messages written before setup are sent once its menus close.
- **`/model` has a tab per provider** with a credential, starting on the
  provider in use. Tab, Shift-Tab, the arrows or a click switch tabs and keep
  what is typed, and a model on another tab switches to that provider without
  signing in again. Each provider's list is fetched on a thread the first time a
  menu shows it and kept for the process, so startup never waits on the network;
  models agt knows itself show at once, and a failed listing is fetched again
  next time and still lets the user type an id. Models show their context window
  and prices, the model in use is tagged on its provider's tab, and those that
  take no images say so. Choosing one resets an effort it does not take. A menu
  keeps its height while its list is filtered and across its tabs.
- **Switching providers or signing in again** mid-session starts a new client,
  so replayed reasoning starts over.
- **`/tasks` lists recent processes** with the newest output of the selected
  one, without consuming the model's unread output. Enter adds its output and
  log path to the transcript, and Ctrl-X stops it as a kill would; the model is
  told with the next request.
- **`/mcp` lists the MCP servers** set up for the working directory, marking
  those the session runs, with the problems in their settings under the list.
  Enter lists a server's tools in the transcript, starting it on a thread, and
  Ctrl-X removes one from agt's own file. The last choice adds a server typed as
  `agt mcp add` takes it, split into words as a shell splits them without
  expanding anything. `/mcp add …` and `/mcp remove <name>` do the same from the
  input. The model's instructions list servers only as the session began, so it
  is told of each change with its next request.
- **`?` in an empty input, or `/help`,** lists the keys.
