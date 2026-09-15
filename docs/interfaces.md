# Interfaces

People and agents reach agt the same ways: its commands, print mode's output,
the files a session writes, a running session's control socket, and the Agent
Client Protocol. The system prompt's `# The agt command` section tells the model
how to run a prompt, follow and steer it, list models and sessions, read a
session and set up MCP servers, so it uses the commands people use.

The commands live in `src/cli/`, the log's records in `src/store.rs`, the
control socket in `src/control.rs`, and ACP in `src/acp.rs`.

## Conventions

- **Plain text for both readers.** What commands print reads the same in a
  terminal and in the output of an agent's command.
- **One parser.** Every command reads its arguments with lexopt, so each takes
  `--name=value`, `-mvalue`, `--`, and options after the words they follow, and
  `agt view` takes paths that are not UTF-8.
- **Only the first argument names a command**, so a prompt may start with the
  same words.
- **Exit codes say whose mistake it was.** A command line a command cannot take
  exits with 2, the problem and the command's help, or for agt itself its usage
  and a pointer to `agt --help`. A command that cannot do what it was asked exits
  with 1 and says why.
- **Settings come from files, the environment and options**, each overriding the
  one before. The variables are listed in the
  [README](../README.md#settings).

## Commands

| Command                                                  | What it does                                                              |
| -------------------------------------------------------- | ------------------------------------------------------------------------- |
| `agt [<prompt>]`                                         | Opens the terminal UI, sending `<prompt>` first                           |
| `agt -p [<prompt>]`                                      | Runs a prompt until the agent is done, reading it from stdin if not given |
| `agt -c`, `agt -r <id>`                                  | Continues the latest session in this directory, or session `<id>`         |
| `agt sessions [--all] [-n <count>]`                      | Lists sessions, newest first                                              |
| `agt sessions show <id> [-f]`                            | Prints a session's transcript, following a running one with `-f`          |
| `agt send [--later] <id> <message>`                      | Sends a message to a running session                                      |
| `agt models [<text>]`                                    | Lists the models you can use                                              |
| `agt models use <model> [--provider <id>] [-e <effort>]` | Saves the model, and its effort, for every frontend                       |
| `agt mcp list`, `tools`, `call`, `add`, `remove`         | Sets up MCP servers and calls their tools                                 |
| `agt view [--region <l,t,r,b>] <image>...`               | Shows images to the session running the command                           |
| `agt login [<provider>] [--key -]`                       | Signs in, in menus or the browser, or saves a key read from stdin         |
| `agt logout <provider>`                                  | Forgets a provider's saved sign-in or key                                 |
| `agt acp`                                                | Serves the Agent Client Protocol over stdin and stdout                    |

`-m`, `--provider` and `-e` choose the model, provider and effort for `agt`,
`agt -p` and `agt acp`, and an effort the model does not take is refused before
anything runs. `agt <command> --help` explains each command.

## Print mode

`agt -p` writes the agent's reply to stdout and the rest to stderr, in plain
lines that read the same in a terminal and in the output of an agent's command,
where the two streams arrive as one:

```
agt: session 1757930000-0a1b2c3d · gpt-5.6-luna low · OpenAI
$ cargo test
  exit 101 · 12.3s · /home/me/.agt/sessions/1757930000-0a1b2c3d/procs/1.log
    thread 'tests::nested' panicked at src/parser.rs:88:5

The nested test fails in parse_list, which stops at the first `]`.

agt: done in 41s · 2 commands, 1 failed · $0.02 · session 1757930000-0a1b2c3d
```

- **The session is named first and last.** The first line names the session,
  which is where a clipped result or an exit notice keeps output, and the last
  names it again after how the turn ended, its time, commands and cost.
- **A call is one line and how it ended.** A command shows its first line
  without a `cd` into the working directory, and other calls what they do, such
  as `> read 3`. Under it come its status, how long it ran and the log file that
  holds all its output, then the headers of the images it showed, or for a
  failed call its last five lines. Calls made together show one at a time, each
  ending under its own command.
- **The reply is the last text.** Text the model writes before calling tools
  shows among the work. A response's text is held until what follows shows
  whether it ended the turn, and the one that did is the reply. Reasoning is left
  out, and notices and errors show as `agt:` lines.
- **Print mode buffers each response until it completes** and discards the
  drafts of failed attempts.
- It exits with 0 when the turn ends normally, and with 1 otherwise.

## Sessions and models

- **`agt sessions`** lists the sessions someone wrote in, newest first, marking
  those running: those of the working directory, or of every directory with
  `--all`, at most 20 unless `-n` says otherwise.
- **`agt sessions show`** opens a log only to read it, so a running session goes
  on undisturbed, and prints it with print mode's transcript from the updates a
  replay produces, messages included. With `-f` it goes on printing what a
  running session appends until its turn ends or the session stops.
- **`agt send`** delivers a message through the session's control socket, for
  its next step or with `--later` once it is done, and `/compact [focus]`
  compacts it. A session that is not running is continued with `agt -p -r`
  instead.
- **`agt models`** lists the models of the provider in use and of each provider
  with a credential, asking the providers that list models at once: each model's
  window, prices, efforts, whether it is in use and whether it takes only text.
  Its first line says which model is in use. Without a text to match ids, it
  lists at most 25 models of a provider and says how many more there are.
- **`agt models use`** saves a model and effort for every frontend, as the
  terminal UI's menus do.
- **`agt login`** runs the terminal UI's setup menus. `agt login <provider>`
  signs in in the browser, or with `--key -` saves an API key read from stdin,
  which keeps it out of the process list.

## The session directory

Each session is a directory of plain files, `~/.agt/sessions/<id>/`:

| File             | Holds                                                                 |
| ---------------- | --------------------------------------------------------------------- |
| `log.jsonl`      | The session log: every item, process, notice, turn and compaction     |
| `procs/<id>.log` | Each process's output as a terminal shows it, as plain text           |
| `images/`        | The images the model saw, by content hash and size                    |
| `notes.md`       | The agent's plan on long tasks, which compaction carries forward      |
| `mcp/<name>.log` | Each stdio MCP server's standard error                                |
| `bin/agt`        | A link to the running agt, when the `agt` on `PATH` is another binary |

## The session log

`log.jsonl` is version 1 of the log. It starts with a header line, then holds
one typed record per line, each event with its time in milliseconds:

```json
{"type":"session","version":1,"cwd":"/work/project","created":1757718902}
{"type":"model","provider":"openai","model":"gpt-6-astra"}
{"type":"item","at":1757718903120,"item":{...}}
{"type":"item","at":1757718904410,"origin":"send","item":{...}}
{"type":"proc","at":1757718905002,"id":1,"command":"cargo test"}
{"type":"exit","at":1757718917310,"id":1,"exit":{"code":101}}
{"type":"notice","at":1757718917311,"text":"process 1 (cargo test) finished with exit 101"}
{"type":"turn","at":1757718930000,"stop":"end_turn"}
{"type":"cost","usd":0.0123}
{"type":"mask","keep":96000}
{"type":"compaction","history":[...],"summary":"...","users":["..."],"provider":"openai","model":"gpt-6-astra","reasoning_from":0,"cost":1.25}
```

| Record       | Written when                                                                                            |
| ------------ | ------------------------------------------------------------------------------------------------------- |
| `session`    | First, once: the log's version, the working directory, and when the session began                       |
| `model`      | The provider or model changes; the items after it came from them                                        |
| `item`       | The model receives or writes an item; `origin` is `send` for a message `agt send` delivered             |
| `proc`       | A command starts as process `id`                                                                        |
| `exit`       | Process `id` exits                                                                                      |
| `notice`     | agt tells the user something, such as a retry or a process finishing                                    |
| `error`      | Something goes wrong                                                                                    |
| `turn`       | A turn ends: `end_turn`, `cancelled`, `max_tokens`, `refusal` or `error`                                |
| `cost`       | A response is priced, in dollars                                                                        |
| `mask`       | Old tool output is elided, keeping the newest `keep` bytes, so a resume elides it at the same point     |
| `compaction` | A compaction leaves a context: all of it, the summary, the user messages taken out, and the cost so far |

Items are what the model received and wrote, and together with processes,
notices, errors and the ends of turns they make the log a complete account that
agents read with `grep` or `jq` and that replays as the session happened. A
record agt does not know is skipped. How the log stays intact and how a session
resumes from it are in [Architecture](architecture.md#what-is-stored).

## Control socket

Each running session serves a Unix socket at `$TMPDIR/agt-<uid>/<id>.sock`, in
a directory only the user can use. It lives there rather than in the session
directory because socket paths are limited to about 100 bytes. Commands in the
session find it in `AGT_SOCKET`, and any process finds a session's socket by the
session's id.

A connection carries one request, a JSON line with an `op`, and its answer,
`{"ok": ...}` or `{"error": "..."}`. Each is served on a thread of its own, so a
slow server holds up no other request, and closing the connection before the
answer cancels the request.

| `op`      | Fields                        | Answer                                                       |
| --------- | ----------------------------- | ------------------------------------------------------------ |
| `servers` | —                             | The session's MCP servers, as `agt mcp list` shows them      |
| `tools`   | `server`                      | A server's tools, starting it if it is not running           |
| `call`    | `server`, `tool`, `arguments` | The tool's result                                            |
| `send`    | `text`, `later`               | `null`, once the message is handed to the agent's event loop |

MCP requests never involve the event loop, and a sent message reaches it as an
event. Whether the socket answers is how commands tell a running session from
one that stopped, and a session whose socket answers is not resumed by another
agt.

## Agent Client Protocol

`agt acp` implements protocol version 1: `initialize`, `authenticate`,
`session/new`, `session/load`, `session/resume`, `session/list`,
`session/close`, `session/delete`, `session/set_config_option`,
`session/prompt`, `session/cancel` and `$/cancel_request`. Each ACP session is
an agent: its updates become `session/update` notifications, and the end of its
turn answers the pending `session/prompt`.

| Agent update                  | ACP                                                                                                                 |
| ----------------------------- | ------------------------------------------------------------------------------------------------------------------- |
| Text and reasoning            | `agent_message_chunk`, `agent_thought_chunk`                                                                        |
| Tool calls                    | `tool_call` of kind `execute`, then `tool_call_update` with live output, the images the output showed, and a status |
| Context usage and cost        | `usage_update`, with `cost` in USD when prices are known                                                            |
| Session title                 | `session_info_update`, from the first prompt                                                                        |
| A message `agt send` delivers | `user_message_chunk`, since the client did not send it                                                              |
| Turn end                      | The `session/prompt` result, `end_turn`, `cancelled` or `max_tokens`, or an error                                   |

- **Malformed requests change nothing.** Parameters are read into types when a
  message arrives, so a malformed request fails with invalid params before it
  changes anything. In ACP mode stdout carries only protocol messages.
- **Prompts** accept text, resource links, embedded resources and images; other
  blocks are rejected. Images are prepared as `agt view` prepares them, on a
  thread so no session waits, and a prompt with an image that cannot be used, or
  for a model that takes none, is rejected with invalid params. Cancelling a
  prompt while its images are prepared answers it as cancelled.
- **Options.** Sessions offer model and reasoning-effort selectors as config
  options. Models come from the provider's catalog and efforts from the current
  model, and changes apply between turns. Discovery runs on a thread and sends a
  `config_option_update` when ready, so opening a session and cancelling never
  wait for the network, and a failed listing is retried when the next session
  opens. A selection must be an advertised option.
- **Commands.** Skills and `/compact` are advertised with
  `available_commands_update`, with an input hint so a request can follow the
  command.
- **Loading and resuming.** A loaded session replays its whole log before the
  response, including the images attached to prompts and shown by commands, and
  calls closed because agt stopped during them; it runs in the requested `cwd`
  and reports its title. A resumed session replays nothing and reads its log
  only from the last compaction. Every opened session reports its context usage
  after the response. Unknown sessions get error -32002. Deleting a session
  closes it if it is open and removes its directory, and deleting one that does
  not exist succeeds.
- **Sign-in.** Without a model and a credential, opening a session fails with
  `auth_required` (-32000). A client that advertises
  `clientCapabilities.auth.terminal` is offered a terminal method that runs
  `agt login`, whose menus exit once they end with a model and a credential;
  the client then starts `agt acp` again.
- **Refusals and errors.** A refusal is reported as `end_turn`, since ACP's
  `refusal` means the prompt was dropped from the conversation, which agt does
  not do. Errors during a turn show as thoughts when they happen, and a turn that
  fails returns the last one. Notices and errors raised outside a turn, such as
  skill warnings, show with the next prompt. A partial response a retry discards
  is marked, since a client's transcript cannot be retracted.
- **agt stays in its own terminals.** It never calls the client's file system,
  terminal or permission methods. `initialize` advertises MCP over HTTP but not
  SSE. The stdio and HTTP servers a session is given join those set up for its
  directory, replacing any of the same name, and an SSE server, or a name agt
  cannot use, is noted with the first prompt.
- **A closed session frees its agent**, and its index is never reused, so late
  events from it are ignored.
