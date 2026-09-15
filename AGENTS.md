# Repository instructions

## Mission

agt is a minimal terminal coding agent with one tool, `bash`, a real terminal,
where `agt view` shows the model images and `agt mcp` calls the tools of MCP
servers. It must be fast, memory-efficient and reliable over sessions that
last weeks. Implement each feature completely and correctly, and add nothing
speculative: every line, dependency and option needs a present use.
[docs/](docs/README.md) describes the design and the invariants below.

## Layout

- `src/main.rs`: the modules and the event wait.
- `src/cli/`: the command line: every command's arguments, help and exit
  codes. `print.rs` is print mode, `render.rs` the text transcript print mode
  and `agt sessions show` share, and each command has a file of its own.
- `src/provider.rs`: the five providers and everything that differs between
  them: endpoints, sign-in, request dialects, native compaction and model
  catalogs.
- `src/config.rs`: saved settings and credentials, environment overrides, and
  the settings each session runs with.
- `src/auth.rs`: API keys, browser sign-in for ChatGPT, Grok and OpenRouter, and
  token refresh.
- `src/models.rs`: model catalogs, reasoning efforts, prices and cost.
- `src/agent/`: the agent state machine shared by every frontend and the
  updates frontends render. `context.rs` is the conversation and its budget,
  `inbox.rs` what waits for the model, `turn.rs` requests and responses,
  `tools.rs` tool calls, `compaction.rs` compaction, and `replay.rs` how a
  logged session shows again.
- `src/llm/`: the Responses API client and retries, request bodies in each
  provider's dialect (`body.rs`) and streamed events (`sse.rs`).
- `src/item.rs`: Responses API items, and the content agt shows the model.
- `src/bash/`: the bash tool: its definition, arguments and result headers
  (`tool.rs`), processes and waits (`mod.rs`), pseudo-terminals (`pty.rs`) and
  the text logs of their output (`log.rs`).
- `src/image.rs`: the images the model sees: preparing, saving, marking and
  sending them.
- `src/control.rs`: each session's control socket, through which `agt send`
  and `agt mcp` reach a running session.
- `src/mcp.rs` and `src/mcp/`: MCP servers: their settings, each session's pool
  of servers, stdio and Streamable HTTP connections in both eras of the
  protocol, and what the instructions list of each server.
- `src/store.rs`: session logs, their typed records and resuming from them.
- `src/prompt/`: the instructions, AGENTS.md discovery (`agents_md.rs`) and
  Agent Skills (`skills.rs`).
- `src/tui.rs` and `src/tui/`: the fullscreen terminal UI: its screen,
  transcript, input and attachments, menus and setup.
- `src/acp.rs`: the Agent Client Protocol server.
- `tests/`: end-to-end tests against the scripted model server and the fake MCP
  server in `tests/support/`.

## Invariants

- The event loop never blocks. Model requests and processes run on threads
  that report through the frontend's channel, and waits are deadlines.
  Nothing wakes while idle.
- Memory is bounded by the context window, not session length. Process output
  lives in log files, the transcript in `log.jsonl`, images in `images/`, and
  only the live context and small per-process state stay in memory.
- Instructions are byte-stable within a session so the provider's prompt cache
  stays valid. Time-varying facts belong in messages.
- Requests are stateless Responses API calls with the full input and items
  kept as opaque JSON. Providers are a closed set of five, and each request
  carries exactly the fields and headers its provider documents.
- The session log is append-only and resumable after a crash at any point.
- In ACP mode stdout carries only protocol messages.
- The terminal UI draws on the alternate screen and writes only the cells that
  changed since the last frame.

## Rust

- Use Rust 2024 and the pinned toolchain. Commit `Cargo.lock`. Add a dependency
  only for a demonstrated need, and prefer the standard library otherwise.
- Name modules for stable responsibilities. Do not add `util.rs`, `helpers.rs`
  or generic `common` modules, or files for individual functions.
- Prefer concrete types and ordinary control flow. Add traits, generics and
  macros only for demonstrated consumers. Keep items private or `pub(crate)`.
- Untrusted input (model output, provider responses, ACP messages, MCP server
  messages, session logs, SKILL.md files) produces typed errors or reported
  problems, never panics. Reserve `expect` for documented internal invariants.
- Comments explain why or record an invariant; they do not narrate the code or
  its history.
- Warnings are errors. Do not silence lints or weaken checks to pass
  verification.
- `unsafe` is forbidden. Process session setup lives in `pty-process`.

## Tests

- Keep unit tests in one `#[cfg(test)] mod tests` at the bottom of the file they
  cover. Do not create `tests.rs` or `*_test.rs` files.
- End-to-end tests in `tests/` run the binary against `tests/support/`, in
  files named for the behavior they cover.
- Parsing tests need independently established expected values. Cover
  malformed, truncated and boundary inputs.
- Keep tests deterministic with bounded waits and isolated temporary
  directories. Never point tests at a real provider or the user's home.
- Goldens in `tests/support/golden/` pin each provider's requests, session
  logs, ACP messages and the terminal UI byte for byte. A change that alters
  one must be intended: rerun with `AGT_BLESS=1` and review the diff.
- Pin each behavior in one place: goldens for exact bytes, unit tests for
  rules and edge cases, end-to-end tests for how the parts connect. Remove a
  check that repeats another rather than keeping it as extra coverage.
- Name a test for the behavior it pins, and make its failure show the values
  that differed: compare with `assert_eq!`, and give other assertions a
  message with the actual value.

## Verification

At each checkpoint run:

```sh
cargo fmt --all --check
cargo lint
cargo test-all
```

Changes to the terminal UI also need a run in a real terminal: streaming, tool
blocks, scrolling, opening entries, resizing, paste and interrupting. Changes
to ACP need a check with an ACP client. Build with `cargo build --release`
when changing dependencies or the release profile, and report binary size or
memory with any performance claim.

## Commits

- Use [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/):
  `type(scope): description`.
- Types: `feat`, `fix`, `perf`, `refactor`, `docs`, `test`, `build`, `ci`,
  `style` and `chore`, chosen by the change's primary purpose.
- Scopes name areas: `agent`, `llm`, `auth`, `models`, `config`, `bash`,
  `image`, `control`, `mcp`, `compact`, `store`, `prompt`, `tui`, `acp`, `cli`,
  `repo` or `deps`.
- Keep the subject at most 72 characters, starting with a lowercase imperative
  verb and without a final period.
- Keep each commit coherent and buildable, with implementation and tests
  together. Include measurements in `perf` commits. Mark incompatible changes
  to the session log format, command-line interface or environment variables
  with `!` and a `BREAKING CHANGE:` footer.
