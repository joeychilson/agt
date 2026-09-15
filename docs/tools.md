# Tools

The model has one tool, `bash`. Three commands of agt's own run through it:
`agt fetch` reads a web page as Markdown, `agt view` shows the model images, and
`agt mcp` calls the tools of MCP servers. No other tool is declared, so a
request is the same size however many pages, images, servers and tools a session
uses.

The tool lives in `src/bash/`, web pages in `src/fetch.rs` and `src/fetch/`,
images in `src/image.rs`, and MCP servers in `src/mcp.rs` and `src/mcp/`.

## The bash tool

| Call                                    | What it does                                                                                                                                        |
| --------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------- |
| `{"command": "npm run dev", "wait": 5}` | Starts `bash -c` in a new 40×120 pseudo-terminal and returns when it exits or `wait` seconds pass (default 30, at most an hour), leaving it running |
| `{"id": 2}`                             | Reads the process's new output                                                                                                                      |
| `{"id": 2, "input": "y"}`               | Types into the process                                                                                                                              |
| `{"id": 2, "kill": true}`               | Sends the process group SIGTERM, then SIGKILL two seconds later                                                                                     |
| `{"wait": 600}`                         | Waits for whatever happens in the background first                                                                                                  |

Typed text is sent as keys. Newlines become Enter, and Enter is added unless
the text ends in a newline, because models expect a typed line to run. Input
holding other control keys is sent exactly as written.

Input and reads return once new output has been quiet for 500 ms, at exit, or
when `wait` passes. A lone `wait` returns with the notices that ended it, such
as a process exiting, or once its seconds pass, and a message sent for the
agent's next step ends it at once. So the model can leave many processes
running, do other work, and then wait for them together.

### Arguments

Arguments are read leniently, since models quote numbers and fill unused fields
with empty strings: a quoted number counts, and an empty string or a zero id is
absent. Unknown arguments, conflicting forms and unknown ids are errors that
name the problem and the call to make instead, because a vague error leads a
model to retry the same call.

The tool is declared with `strict: false`. Otherwise OpenAI's models treat every
argument as required and fill the unused ones, such as a placeholder command in
a call meant to read a process, which makes the call ambiguous. In a live
comparison GPT-5.6 Luna, GPT-5.6 Sol and GPT-6 Astra filled them in every call
without it, and in none with it.

### Results

A result starts with a header saying what it is of, followed by the output that
is new since the last read:

```
[id 3 · exit 0 · 1.2s]
[id 4 · running · 30.0s · you will be told when it exits]
[id 5 · running · 30.0s · process 2 already runs this command · you will be told when it exits]
[waited 12.3s]
```

- Output is rendered as a terminal shows it: escape sequences are dropped and
  carriage returns overwrite. A long result keeps 4 KB of head and 8 KB of
  tail, cut at line boundaries, and says which log holds the rest.
- While a full-screen program is active, the result is its screen instead.
- A new command still running is told its exit will be reported. One that a
  running process already runs names that process, since models sometimes lose
  track of a command left running and start it again.
- A command that only `cat`s a catalog skill's `SKILL.md` returns the skill's
  full instructions, which are never elided as old output. Reading a skill already loaded
  returns a short note instead.

### Logs

Each process's output is kept in `procs/<id>.log` in the session directory, as
a terminal would show it. A reader thread passes output through a terminal
parser: escape sequences are dropped, and carriage returns and cursor moves
overwrite the unfinished line, which is rewritten in the file in place. A
progress bar therefore leaves one line rather than hundreds, and `cat`, `grep`
and `tail -f` read the file as they would a terminal's scrollback. A line the
model has read that changes afterwards, as a progress line does, is read again
whole.

The same thread feeds a `vt100` screen for full-screen programs and writes the
header of each image `agt view` shows in place of its marker. A waiter thread
reports the exit after giving the reader up to 200 ms to drain. A log restarts
at 64 MB, and the model is told how much earlier output was discarded.

### Processes

- **No limit on how many.** Each process holds a pseudo-terminal and two
  threads, and agt raises its open-file limit to the hard limit (10240 on macOS)
  when it starts the first. An exited process releases its terminal and screen
  at once. Beyond 64 entries the oldest exited ones are forgotten, fully read
  ones first, unless a call still waits on them; their logs stay on disk.
- **Signals reach the process group only until the process is reaped**, since
  the group id can be reused after that.
- **Input never blocks the agent.** A thread per process writes it, with at most
  one write queued, so a process that stops reading cannot pile up input; the
  next write reports that the previous input has not been read.
- **The environment is set for a non-interactive reader.** Commands get
  `TERM=xterm-256color`, `PAGER=cat`, `GIT_PAGER=cat` and `NO_COLOR=1`, the
  session directory in `AGT_SESSION_DIR` and its control socket in
  `AGT_SOCKET`, and not `AGT_API_KEY`.
- **`agt` is always the running agt.** `agt view` must write the markers this
  session reads and `agt mcp` must reach its socket, so unless the `agt` on
  `PATH` is the running binary, the session's `bin/`, where `agt` links to it,
  leads `PATH`. The tool's description says `agt` is on `PATH`, so models need
  not check.

## Images

The model sees an image by running `agt view <file>...`, or `agt view --region
left,top,right,bottom <file>` for part of one at full resolution: on its own,
after the command that made the image, or in a loop. The tool's description
explains this to models that accept images: OpenAI's, listed models whose input
modalities include images, and ids no listing describes.

1. `agt view` prepares and saves each image in its own process, so decoding and
   scaling, which take a good part of a second for a large image, never hold up
   agt.
2. It writes a marker to its controlling terminal, the command's
   pseudo-terminal: `ESC ] 7719 ; <saved file> ; <header> BEL`. It writes to the
   terminal rather than stdout so that pipes, redirections and command
   substitution cannot take the marker. Errors go to stderr with a failing exit,
   as any command's do.
3. The reader thread finds markers as output arrives, even split across reads,
   and writes each image's header into the log on a line of its own in the
   marker's place. A marker that names no file directly in `images/`, or holds a
   control character, stays ordinary output. The log keeps headers rather than
   markers, so reading a log again shows no image.
4. Results, reads of running processes and exit notices carry the images shown
   in the output they return, each right after its header. Output that shows
   images is logged as `input_text` and `input_image` parts; other output stays
   a string.

An image is prepared in these steps:

| Step   | What happens                                                                                                                                                                               |
| ------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Decode | PNG, JPEG, WebP, and the first frame of a GIF, with the `image` crate                                                                                                                      |
| Shape  | Turned upright by its EXIF orientation and cut to the region, which stops at the edges                                                                                                     |
| Scale  | To fit 2000 px per side and 2,500 patches of 32 px: the most OpenAI's models take at high detail without scaling again, and within what Claude takes in a request of more than 20 images   |
| Encode | Kept as its own file when nothing changed it; otherwise PNG, or JPEG at quality 85 when the original is a JPEG or the PNG would pass 3 MB, under Claude's 5 MB limit on Bedrock and Vertex |
| Save   | As `images/<hash>-<width>x<height>.<ext>` in the session directory                                                                                                                         |

- **A header names the image**, such as
  `[image /work/shot.png · 2880x1800 · shown at 1980x1238, scale 1.45]`, with
  the original size and the scale for mapping coordinates back. It stays
  wherever the image cannot go: summaries, checkpoints and elided output.
- **History holds names, not pixels.** Each request reads the saved file into a
  data URL, so the log and memory hold only names, a screenshot overwritten
  later cannot change what the model saw, and identical images share a file. A
  missing file becomes a note, and a name refers only to a file directly in
  `images/`, so a damaged log cannot point a request anywhere else.
- **Paths are found as models write them.** A path under a quoted `~`, which
  the shell leaves unexpanded, is looked for under the home directory. A name
  that does not exist is looked for among names in its directory that differ
  only in their kinds of spaces and quotes: macOS names screenshots with a
  narrow no-break space before AM or PM, which models type as a plain space.
- **Errors say what to do.** A directory gets an error saying so, and a file
  that is not an image an error naming a converter: `pdftoppm` for PDF pages,
  `rsvg-convert` for SVG, and `sips` or `magick` for anything else.
- **A rejected image is not sent again.** A provider that rejects an image
  despite these checks is sent no more images in the session: a notice says so,
  images in history become notes, and the request is sent again. Switching
  models or resuming tries images again.

## Web pages

`agt fetch <url>` reads a page as Markdown. A page is read from the best source
it has, and a source that fails gives way to the page itself, so a rule that
does not fit a page costs one request and nothing more.

| Source                | Where it comes from                                                                          |
| --------------------- | ---------------------------------------------------------------------------------------------- |
| The site's Markdown   | Every request asks for `text/markdown` first, which many documentation sites answer with     |
| `llms.txt`            | The root page of a site that has one                                                         |
| The Markdown version  | HTML with `<link rel="alternate" type="text/markdown">`, or a link to the page's own llms.txt |
| A first-party source  | The kinds of pages `src/fetch/sites.rs` knows                                                |
| The page              | Everything else, whose main content becomes Markdown                                         |

The kinds of pages read from a source of their own: a code host's view of a file
from the raw file, on GitHub, GitLab, Codeberg and Hugging Face; a GitHub
repository, directory, issue or pull request from GitHub's API, with the token
`GH_TOKEN` or `GITHUB_TOKEN` holds, since GitHub allows 60 requests an hour
without one; an arXiv abstract or PDF from the paper's HTML; a DOI from the
publisher's page, asked for as HTML because doi.org answers a request for
Markdown with a citation; an npm, PyPI or crates.io page from the registry's
metadata and README; and a Stack Exchange question from the API, with its
answers.

### What the model sees

In a session the page is saved in `fetch/` in the session directory and printed
after a header saying where it came from and where it is saved:

```
[https://bun.com/docs · its Markdown version https://bun.com/docs.md · 131 lines · 6.0 KB · saved as ~/.agt/sessions/<id>/fetch/bun.com-docs-4ba1f20c.md]
```

A page longer than a result shows keeps its header, then its headings with their
line numbers and its first lines, so the model reads the rest of the file in
ranges or searches it. Piped, and outside a session, the Markdown is printed
alone. A response that is not text, such as a PDF or an image, is not read: the
error names its type and size and says to download it with `curl`. A page that
shows its content with JavaScript has no text to read, and says so.

### HTML as Markdown

The main content is what readability finds, or else the largest `main`,
`article` or `[role=main]`, with navigation, sidebars and other chrome removed.
It is written for a reader:

- **One block to a line, and no wrapping**, so a line number or a search finds
  it, under the page's title, taken from its `h1` when the `<title>` holds that
  and otherwise from the `<title>` without the site's name.
- **Links absolute**, with permalinks, links without text and images without
  descriptions left out; an image inside a link reads as its description.
- **Code fenced** with the language its classes name, keeping its lines when a
  highlighter puts each in an element of its own, and dropping line numbers.
- **Tables as tables**, unless a table lays out a page, whose cells are then
  read as the blocks they hold.
- **Nothing a terminal acts on**: control characters are dropped, and the
  characters that would begin a block the HTML does not have are escaped.

## MCP servers

`agt mcp tools <server> [<tool>]` shows a server's tools as signatures such as
`navigate(url: string, wait?: number)`, and `agt mcp call <server> <tool>
<json>` calls one. The commands work in a session and outside one.

### Settings

Servers come from `~/.agt/mcp.json`, then the nearest `.mcp.json` from the
working directory up to the repository root, then those an ACP client gives,
each replacing a server of the same name. Files use the `mcpServers` shape other
agents write: `command`, `args`, `env` and `cwd`, or `type: "http"`, `url` and
`headers`, with `${NAME}` and `${NAME:-default}` read from the environment. A
server with a problem is left out and the problem reported.

`agt mcp add` and `remove`, and the terminal UI's `/mcp`, edit agt's own file,
which only the user can read, since headers and environments hold tokens.

### What the model is told

The instructions list each server in an `<available_mcp_servers>` block with
what it said it is for, its description or the first paragraph of its
instructions, and its tools' names, within min(16 KB, B/16 tokens) of the
context budget B, leaving out tool names first and then servers. These come
from `~/.agt/mcp/<name>-<hash>.json`, keyed by a hash of how the server is
reached and written whenever agt starts a server or lists its tools. Opening a
session therefore starts no server, and the instructions never depend on one
answering; a server never used is listed by its name.

### Running servers

- **A server runs until the session ends.** In a session, `agt mcp` sends its
  request to the session's [control socket](interfaces.md#control-socket), and
  the session's pool starts a server the first time a command uses it and keeps
  it running, so state such as a browser page carries from call to call.
  Settings are read again for each request, so a server added during a session
  can be used at once, and one whose settings changed is started again.
- **A stdio server runs apart.** It runs in the working directory, in a process
  group of its own, without `AGT_API_KEY`, with its standard error in
  `mcp/<name>.log` in the session directory. Stopping it closes its input, then
  sends the group SIGTERM and SIGKILL two seconds apart. A server that exits is
  started again by the next call, and calls waiting on it fail with the end of
  its log.
- **An interrupted command cancels its request.** A command that ends before
  its answer, as when the model interrupts it, closes its connection. A stdio
  server is then sent `notifications/cancelled` within 100 ms, and an HTTP
  response is dropped when it next delivers anything, since a blocked read
  cannot be interrupted.
- **Each HTTP message goes on a connection of its own.** A server may close a
  connection it has answered without saying so, and a message sent on it then
  would be lost, while sending it again could call a tool twice.
- **Outside a session** `agt mcp` starts the servers it uses itself, and their
  input closes when it exits.
- **Waits are bounded where nothing else bounds them.** A server has 30 s to
  start and answer its first request, and falling back to `initialize` gets 30 s
  more. A tool call waits as long as the command that makes it.

### Calls and results

A call's arguments are checked for the properties the tool's schema requires,
apart from any with a default: servers fill those in, although generated
schemas often require them, and signatures show them as optional. An unknown
tool is refused with the names close to it, so a mistaken call is corrected
without reaching the server, and a tool missing from the last listing is looked
for again, since servers change their tools.

A result prints its text. Its images are prepared, saved and shown to the
session as `agt view` shows them, so they reach the model after their headers,
and audio and binary resources are noted. A result the server marks as an error
exits with status 1.

### Both eras of the protocol

|                   | Modern (2026-07-28)                                                                                                                                        | Legacy (2025-11-25 and earlier)                                              |
| ----------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------- |
| Session           | None: every request carries `_meta` with the protocol version, client information and capabilities                                                         | The `initialize` handshake                                                   |
| Over HTTP         | `Mcp-Method`, `Mcp-Name` and `Mcp-Param-*` mirror the method, tool and arguments the schema marks with `x-mcp-header`, Base64-encoded when not plain ASCII | The session id the server issues goes with every later request               |
| Recovery          | A tool with invalid annotations is left out; a call rejected for mismatched headers lists the tools again and is retried                                   | A 404 starts a new session and the request is sent again                     |
| Server's requests | A result that asks for input fails, naming what it asked for                                                                                               | `ping` is answered with an empty result, anything else with method not found |

As the specification advises, a connection sends `server/discover` first and
falls back to `initialize` on any reply but a result or a modern error. A server
remembered as legacy is initialized first.
