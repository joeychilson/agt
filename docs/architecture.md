# Architecture

agt is one binary: three frontends over one agent core. The core runs the
model's turns and the commands it calls, keeps the conversation within its
context, and writes everything the session does to the session's directory. The
frontends, the terminal UI, print mode and the Agent Client Protocol, pass it
events and render what it reports.

## The event loop

```
terminal input thread ─┐
ACP stdin thread ──────┤
model stream thread ───┤
process reader × N ────┼─► channel ─► Agent::handle ─► updates ─► TUI | ACP | print
process waiter × N ────┤                ▲
control socket ────────┘      Agent::poll, then recv_timeout(next deadline)
```

The frontend owns the channel and the loop. The agent never blocks: model
requests and processes run on threads that send events, and every wait is a
deadline. `Agent::poll` runs what is due and returns the next deadline, which
the loop honors with `recv_timeout`. With no deadline the loop blocks in `recv`,
so an idle agent uses no CPU.

## Turns

The agent is a state machine, `Idle`, `Responding`, `Tools` or `Compacting`,
whose state has parts with one job each: the conversation and its budget
(`context.rs`), what waits for the model (`inbox.rs`), the session's processes
(`bash::Procs`), its log (`store::Session`), and the turn in progress
(`turn.rs`, `tools.rs` and `compaction.rs`).

1. A message starts a turn, and the agent sends a request with the whole
   context.
2. A response with function calls starts the calls and enters `Tools`. When
   every call has a result, the outputs are appended in call order and the next
   request is sent.
3. A response without calls ends the turn.

- **Late events are ignored.** Model events carry the epoch of the request that
  produced them. Cancelling or restarting bumps the epoch, so late events from
  an abandoned request are ignored, and dropping the `llm::Stream` closes its
  connection.
- **A message sent while the agent works waits for its delivery.** One for the
  next step goes out with the next request, once the running calls finish, and
  no call keeps it waiting more than three seconds: a command still running then
  keeps running, and its exit is reported as usual. One for later waits until
  the turn ends and then starts a turn of its own, one message per turn.
  Cancelling hands the waiting messages back to the frontend unsent, and the
  frontend can take back the newest one while it waits. A message `agt send`
  delivers is handled as the user's, and logged and shown as sent.
- **The background reaches the model as notices**: a process left running that
  exits, one the user stops, processes a resumed session lost, and MCP servers
  the user adds or removes. Notices go out with the next tool result or request
  in a `<background>` block, each line stamped with the UTC date and minute it
  happened, since a session spans days. An exit notice names the process's log
  and carries its unread output, up to 1 KB of head and 3 KB of tail, which
  saves the model a request to read it. In the terminal UI an exit starts a
  turn, at once while idle or when the current turn ends.
- **A resumed history is valid input.** On resume, function calls left without
  outputs by a crash get an "interrupted" output, and clients see them fail.
- **Frontends never read the log.** They render the agent's updates. A call
  reaches them parsed once, as the tool and arguments it names, and a message
  when it enters the history, where the model receives it. A logged session is
  replayed as the updates it produced live, so a resumed session looks as it
  did.

## Context and compaction

A session's context is kept within a budget B: the context window W, or the
model's pricing tier when that is smaller, since a request past the tier is
billed at higher rates for all of its tokens. A request compacts first when the
context passes the lower of W minus a reserve of max(0.15 W, 16k), at most W/2,
and 95% of the tier. A new user turn compacts first at three quarters of that,
so compaction tends to fall between tasks rather than in the middle of one.

How a context is compacted depends on the provider:

- **OpenAI compacts inline.** Every request carries
  `context_management: [{"type": "compaction", "compact_threshold": N}]`, with
  the new-turn threshold at the start of a turn and the full one within it, and
  the provider compacts while it responds. A response that carries a compaction
  item replaces the context from that item on, as OpenAI advises for requests
  that carry the whole input, and is recorded like any compaction. Every request
  carries every image in the context, so agt itself elides old output only once
  those pass 8 MB.
- **ChatGPT, Grok, and OpenAI's models on Vercel AI Gateway compact through
  `/responses/compact`** once the context passes the threshold. The endpoint
  receives the normal instructions, tools, reasoning settings and history, and
  returns the next context: an encrypted compaction item carrying the model's
  own state, which OpenAI precedes with the user's messages. That output is used
  exactly as returned, as both providers require. A response without a
  compaction item, or any failure, falls back to a summary, as does a focus
  given with `/compact`.
- **Other models compact with summaries.** If replacing old tool outputs with
  their status line and a pointer to the log, and their images with the names of
  their saved files, would bring the context under B/2, that is all that
  happens. The newest min(B/8, 24k) tokens of output and loaded skills are kept,
  and once the images every request carries pass 8 MB, old output is elided the
  same way before the next request, however much context is left. Otherwise the
  conversation is summarized first, while its prefix is still cached, and old
  outputs in the kept tail are elided afterwards.

A provider's compaction cannot know where the transcript is, the time, the
running processes, the agent's notes or the skills in use, so agt tells the
model in a message right after the compaction item. The compaction record keeps
that message, so a session that ends at once, as `agt -p` does, still passes it
on when resumed.

### Summaries

A summary is written in one of two ways, the second when the first fails:

1. With the session's normal instructions, tools and history, plus one closing
   message and `tool_choice: "none"`, so the provider serves the conversation
   from its cache.
2. If that fails, overflows, or the model calls a tool anyway, as a separate
   request: the dropped span serialized as text, with tool outputs clipped to
   2000 bytes and the newest entries within B/2, and the previous summary.

Summaries cut the history at a turn boundary, a user message or the first item
of a model response, so a call is never separated from its output or its
reasoning. The cut keeps about min(B/8, 24k) tokens of recent history. Both
requests use a fixed section template and a limit of B/40 words, between 100
and 1500, and ask the model to update the summary rather than rewrite it. The
template's Current state section says where the work stands, because agents
resuming from summaries otherwise lose track of their last step. Summary output
is capped at min(B/4, 16k) tokens, leaving room for reasoning without
exhausting a small window.

A summary's new context is a checkpoint message followed by the kept tail. The
checkpoint holds:

| Part                                                                                              | Kept within                                                                                 |
| ------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------- |
| The path of the full transcript, which the agent can search                                       | —                                                                                           |
| The user's messages verbatim: the first and the most recent                                       | min(24 KB, B/16 tokens), the first clipped to a third of that and each later one to a sixth |
| The summary                                                                                       | B/40 words                                                                                  |
| The instructions of skills in use; skills that do not fit are named for the model to read again   | min(100 KB, B/10 tokens)                                                                    |
| State gathered by code: working directory, `git status`, recent commits, running processes, notes | The last 4 KB of `notes.md`                                                                 |

The clipping keeps a long paste from crowding out the other messages. Git runs
without optional locks, so it never blocks the user's own git commands, on a
thread of its own while the summary is written; if it has not finished, the
checkpoint leaves out its state. `notes.md` is read from its end without loading
the whole file. Instructions are rebuilt on the same thread, picking up edits to
AGENTS.md and skills, and if that has not finished the current instructions
stay. Messages and notices that arrived meanwhile go out with the next request.

If summarizing fails, old tool output is elided and the turn continues without
a summary. After a failed compaction, agt waits until the context has grown by
another min(B/8, 24k) tokens before trying again, and if a failed compaction is
followed by an overflow, the turn fails instead of looping.

## Prompt caching

Cached input costs a fraction of fresh input, so requests are shaped for it:

- **Instructions and tools stay byte-identical** until compaction; only a switch
  to a model that differs in taking images changes the bash tool's description.
  Sections run from most to least widely shared, behavior and the agt command,
  skills, MCP servers, AGENTS.md, then the session's directories, so a new
  session can reuse a cached prefix.
- **History is only appended to between compactions.** Eliding and summarizing
  rewrite it together, at the one point where the cache breaks anyway.
- **Each provider is asked in its own way.** The session id keys its cache and
  session affinity, and requests to Anthropic's models ask for caching, as
  [Providers](providers.md#what-a-request-carries) describes.

## What is stored

Everything a session does is written to its directory, `~/.agt/sessions/<id>/`,
as it happens: the log, a text log per process, the images the model saw, the
agent's notes, and MCP servers' standard error. [Interfaces](interfaces.md#the-session-directory)
lists the files and the log's records. Memory holds only the live context and a
little state per process, so it is bounded by the context window rather than by
how long the session has run.

- **The log is append-only.** Torn final lines are skipped, and a newline is
  restored before the next append, including after a failed write. When a write
  fails, the turn stops before its next request or command and reports the
  failure, so the log never misses work that happened.
- **Resuming reads from the last compaction.** A compaction record holds the
  whole context it leaves, the provider and model whose reasoning that context
  replays, and the session's cost so far, so resuming never reads before it. agt
  searches back from the end of the log for a line starting with
  `{"type":"compaction",`, which no string can hold at a line start because JSON
  escapes newlines, and reads from there, so startup stays fast however long a
  session has run. A record that does not parse, as after a crash while writing
  it, falls back to reading the whole log. Loading a session for an ACP client
  reads everything, since every item is replayed to it.
- **Elided output is not held twice.** A mask record holds the budget in bytes
  it kept, and resuming reapplies it at that point, so discarded tool output does
  not pile up in memory between summaries. The original output stays in the
  process logs.
- **One agt writes a session.** A resumed session runs in the directory it is
  resumed from. A session whose control socket still answers is running in
  another agt and is not resumed, since two agents appending to one log would
  each lose the other's work.

## Instructions, AGENTS.md and skills

- **AGENTS.md files** are read from `~/.agt/AGENTS.md`, then one per directory
  from the git root down to the working directory (`AGENTS.md`, else
  `CLAUDE.md`), most general first. A budget of min(32 KB, B/8 tokens) is spent
  on the most specific files first, since they take precedence. Up to 50
  AGENTS.md files elsewhere in the repository are listed by path for the model to
  read before working under their folders.
- **Skills** are the immediate subdirectories holding a `SKILL.md` under
  `.agents/skills` and `.claude/skills` from the working directory up to the git
  root, then `~/.agt/skills`, then the same two directories under the home
  directory. The first skill with a name wins; shadowed duplicates are reported,
  and identical copies are not. Validation is lenient, as the Agent Skills
  integration guide recommends: bad or missing names warn, and a missing
  description skips the skill. Frontmatter is read for plain, multi-line, quoted
  and block scalars.
- **The skill catalog** is an `<available_skills>` block with each skill's name,
  description and `SKILL.md` location, kept within min(16 KB, B/16 tokens) by
  shortening descriptions and then listing fewer skills. The model activates a
  skill by reading that file. `/name` or `$name` in text the user typed inlines
  the skill's instructions, wrapped in `<skill_content>` with its directory.
- **Reads are bounded before allocation**, for instruction and skill files
  alike.

## Configuration

`config.rs` resolves one `Config` for every frontend. The environment overrides
the saved settings, and `--provider`, `--model` and `--effort` override both. A
session runs with the `Settings` it takes from them: the endpoint and
credential, the model, the effort and the window. New settings apply between
turns and change only what differs: a new endpoint starts a new client, a new
endpoint or model starts replayed reasoning over, and a new provider, model or
window resets the context limits and whether images are sent.

- **`config.json`** holds the model, with its provider, context window, efforts,
  pricing tier, prices and whether it takes images, and the reasoning effort. A
  saved model applies only on its provider.
- **Efforts are a closed set**, from `none` to `max`. `AGT_REASONING` must name
  one, and an effort saved or listed that agt does not know is left out.
- **Saves are atomic.** `config.json` and `auth.json` are replaced by renaming a
  new file over them, saves within a process take turns, and fields agt does not
  use are kept.
- **The context window** is `AGT_CONTEXT_WINDOW`, else the model's.
- **Without a model**, print mode and ACP sessions fail with an error saying how
  to choose one, and the terminal UI opens its setup menus.

Credentials, sign-in and model catalogs are in [Providers](providers.md).

## Module layout

From the frontends inward:

| Module           | Responsibility                                                                     |
| ---------------- | ---------------------------------------------------------------------------------- |
| `main.rs`        | The modules and the event wait                                                     |
| `cli/`           | Every command's arguments, help and exit codes; print mode and its text transcript |
| `tui.rs`, `tui/` | The terminal UI: its screen, transcript, input, attachments, menus and setup       |
| `acp.rs`         | The Agent Client Protocol server                                                   |
| `agent/`         | The state machine every frontend shares, and the updates they render               |
| `llm/`           | The Responses API client and retries, request bodies and streamed events           |
| `item.rs`        | Responses API items, and the content agt shows the model                           |
| `bash/`          | The bash tool: its definition, processes, pseudo-terminals and logs                |
| `image.rs`       | Preparing, saving, marking and sending images                                      |
| `mcp.rs`, `mcp/` | MCP settings, each session's pool, and connections in both eras                    |
| `control.rs`     | Each session's control socket                                                      |
| `store.rs`       | Session logs, their records, and resuming from them                                |
| `prompt/`        | The instructions, AGENTS.md discovery and Agent Skills                             |
| `provider.rs`    | The five providers and everything that differs between them                        |
| `config.rs`      | Saved settings and credentials, and environment overrides                          |
| `auth.rs`        | API keys, browser sign-in and token refresh                                        |
| `models.rs`      | Model catalogs, reasoning efforts, prices and cost                                 |

## Measured

On an Apple M2 with 16 GB of memory, running the release binary against a local
scripted server that answers at once, so the times are agt's own:

| Operation                               | Time               | Memory         |
| --------------------------------------- | ------------------ | -------------- |
| Start and exit (`agt --version`)        | ~4 ms              | 2.0 MB         |
| A print-mode turn that runs one command | ~9 ms              | 4.3 MB         |
| List 20 sessions (`agt sessions`)       | ~5 ms              | 2.8 MB         |
| Terminal UI at rest for 10 s            | under 10 ms of CPU | 2.8 MB         |
| Release binary                          | —                  | 3.9 MB on disk |

Times are medians of 20 to 30 runs at a load average of two to four, and a
quieter machine is faster. Memory is the peak resident size, and for the
terminal UI its resident size after 10 s at rest. The print-mode turn includes
two model requests, starting `bash` in a pseudo-terminal, and writing the
session's log.
