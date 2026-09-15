//! The instructions every request carries: how agt works, the skills and MCP
//! servers a session has, the project's AGENTS.md files and the session's
//! environment.
//!
//! Instructions are built when a session starts and rebuilt at compaction,
//! when the prompt cache is invalidated anyway; unchanged files produce the
//! same bytes. Sections run from most to least widely shared (how agt works,
//! then skills and servers, the project's instructions, and this session's
//! directories), so providers can reuse a cached prefix across sessions.
//! Anything that changes during a session reaches the model as a message.

mod agents_md;
mod skills;

use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::{mcp, models, store};
pub(crate) use skills::{SKILL_CONTENT, Skill, skill_block, skill_names, user_words};

/// Size of each catalog, of skills and of MCP servers, which every request
/// carries.
const CATALOG_BUDGET: usize = 16 * 1024;
/// Characters of each description kept when a full catalog does not fit.
const CATALOG_DESCRIPTION: usize = 250;

const BEHAVIOR: &str = "\
You are agt, a coding agent working in the user's terminal. Your one tool, bash, is a real terminal on the user's machine: you read and edit files, build, test and run programs, use git, look at images and call MCP servers by running commands.

# How you work
- Carry the task through. Investigate, make the change, and verify it by running it: the tests, the build, the program itself. Then fix what broke. Stop only when it is done or when something needs a decision only the user can make, and then ask exactly that.
- Read the code before changing it, follow its conventions, and keep the change to what the task needs.
- Write a new file with a quoted heredoc (cat > file <<'EOF'). Make a targeted edit with a short python3 or perl script, or sed for a single line, and check it with git diff.
- Calls in one response run at the same time, so put independent commands in parallel calls.
- Give a command a wait long enough to finish. A command that outlives its wait keeps running and you are told when it exits; if you need its result before you reply, wait for it with {\"wait\": N}.
- This is the user's real machine and real repositories. Take steps outside the task that are hard to undo, such as pushing, publishing, deleting data you did not create or rewriting shared history, only when the user asked for them.
- The user watches your commands as they run, so do not narrate them. When you finish, reply in a few sentences: what changed, how you verified it, and anything left. Use Markdown where it helps, and refer to code as path:line.
- The user can message you while you work; take a message that arrives mid-task into account at once. What happens in the background reaches you in <background> blocks, each line stamped with the time it happened.

# The agt command
agt is on PATH in your commands, and agt <command> --help explains each one.
- agt view <image>... shows you images; the bash tool's description says how.
- agt mcp tools <server> and agt mcp call <server> <tool> '<json>' use MCP servers.
- agt -p '<prompt>' runs a prompt in a new agt session until its agent is done. It prints the session id, each command that agent runs with how it ended and its log file, and last the reply. Run it as a background command to work in parallel, read its output as it goes, and steer it with agt send <session> '<message>'. -m <model>, --provider <provider> and -e <effort> choose its model; agt models lists them. agt -p -r <session> '<prompt>' continues a session.
- agt sessions lists sessions. agt sessions show <session> prints one, and with -f follows a running session until its agent is done.
- agt mcp add, agt mcp remove, agt login and agt models use change the user's setup when they ask.

# Long tasks
A session can run for days. As the context fills, earlier conversation is compacted: you keep your recent work and a condensed record of the rest, and the full transcript stays in log.jsonl in the session directory, where you can search it. On any task longer than a few steps, keep notes.md in the session directory current with the goal, the plan, decisions made and where you are. It is carried through every compaction.";

/// Where a session runs, which its instructions describe.
#[derive(Clone, Debug)]
pub(crate) struct Place {
    pub(crate) cwd: PathBuf,
    /// agt's own directory, which holds global skills and AGENTS.md.
    pub(crate) home: PathBuf,
    /// The user's home directory, when known.
    pub(crate) user_home: Option<PathBuf>,
    /// The session's directory.
    pub(crate) session: PathBuf,
    /// When the session was created, in seconds since the Unix epoch.
    pub(crate) created: u64,
}

/// A session's instructions and what they list.
pub(crate) struct Instructions {
    pub(crate) text: String,
    pub(crate) skills: Vec<Skill>,
    /// The MCP servers, as the instructions list them.
    pub(crate) servers: Vec<mcp::Listing>,
    /// Problems found while loading, for the user.
    pub(crate) warnings: Vec<String>,
}

/// Builds the instructions of a session at `place` with a context of `budget`
/// tokens, listing MCP `servers`.
pub(crate) fn build(place: &Place, budget: u64, servers: &[mcp::Server]) -> Instructions {
    let mut text = String::from(BEHAVIOR);
    let (skills, warnings) = skills::discover(&place.cwd, &place.home, place.user_home.as_deref());
    let size = models::bytes(budget / 16).min(CATALOG_BUDGET);
    if !skills.is_empty() {
        text.push_str(&skills::catalog(&skills, size));
    }
    let listings = mcp::listings(&place.home, servers);
    if !listings.is_empty() {
        text.push_str(&server_catalog(&listings, size));
    }
    text.push_str(&agents_md::section(&place.cwd, &place.home, models::bytes(budget / 8)));
    let _ = write!(
        text,
        "\n\n# Environment\n\
         - Working directory: {cwd}\n\
         - Platform: {os} ({arch})\n\
         - Session started: {started}; agt reports times in UTC\n\
         - Session directory: {session}, also $AGT_SESSION_DIR\n  \
           - log.jsonl: the full transcript, one JSON record per line\n  \
           - procs/<id>.log: the complete output of process <id>, as plain text\n  \
           - notes.md: yours to keep",
        cwd = place.cwd.display(),
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        started = store::timestamp(place.created.saturating_mul(1000)),
        session = place.session.display(),
    );
    Instructions { text, skills, servers: listings, warnings }
}

/// The catalog of MCP servers within `budget` bytes: each with what it is for
/// and the tools it had when last used. Tool lists go first when it does not
/// fit, then servers.
fn server_catalog(listings: &[mcp::Listing], budget: usize) -> String {
    let entry = |listing: &mcp::Listing, tools: bool| {
        let mut entry = format!("\n<server>\n<name>{}</name>", xml_escape(&listing.name));
        if let Some(about) = &listing.about {
            let about: String = about.chars().take(CATALOG_DESCRIPTION).collect();
            let _ = write!(entry, "\n<description>{}</description>", xml_escape(&about));
        }
        if tools && !listing.tools.is_empty() {
            let _ = write!(entry, "\n<tools>{}</tools>", xml_escape(&listing.tools.join(", ")));
        }
        entry.push_str("\n</server>");
        entry
    };
    let mut entries: Vec<String> = listings.iter().map(|listing| entry(listing, true)).collect();
    if entries.iter().map(String::len).sum::<usize>() > budget {
        entries = listings.iter().map(|listing| entry(listing, false)).collect();
    }
    let mut out = String::from(
        "\n\n# MCP servers\n\
         MCP servers give you the tools of other programs and services, through bash. Before you use a server, run agt mcp tools <server> for its tools' descriptions and arguments. Call a tool with agt mcp call <server> <tool> '<arguments as a JSON object>', or pass long arguments on stdin with - and a quoted heredoc. A call exits with status 1 when the tool reports an error. Servers keep running through the session, so state such as an open browser page carries from one call to the next, and images a tool returns are attached to the result.\n\n\
         <available_mcp_servers>",
    );
    let listed = fill(&mut out, &entries, budget);
    out.push_str("\n</available_mcp_servers>");
    if listed < listings.len() {
        let _ = write!(
            out,
            "\n{} more servers are set up but not listed; agt mcp list names them.",
            listings.len() - listed
        );
    }
    out
}

/// Appends the `entries` that fit in `budget` bytes, in order, and returns how
/// many did.
fn fill(out: &mut String, entries: &[String], budget: usize) -> usize {
    let mut used = 0;
    entries
        .iter()
        .take_while(|entry| {
            used += entry.len();
            used <= budget
        })
        .map(|entry| out.push_str(entry))
        .count()
}

/// The nearest ancestor of `cwd` (inclusive) that is a git work tree root.
fn project_root(cwd: &Path) -> Option<&Path> {
    cwd.ancestors().find(|dir| dir.join(".git").exists())
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Reads at most `limit` bytes of a file, bounding reads before allocating.
/// A UTF-8 character cut at the limit is left out, while malformed bytes in
/// the file are reported. Returns whether the file was longer.
fn read_prefix(path: &Path, limit: usize) -> io::Result<(String, bool)> {
    let mut bytes = Vec::new();
    fs::File::open(path)?.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    let truncated = bytes.len() > limit;
    bytes.truncate(limit);
    if let Err(error) = std::str::from_utf8(&bytes) {
        if truncated && error.error_len().is_none() {
            bytes.truncate(error.valid_up_to());
        } else {
            return Err(io::Error::new(io::ErrorKind::InvalidData, error));
        }
    }
    String::from_utf8(bytes)
        .map(|text| (text, truncated))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        fs::write(path, text).expect("write");
    }

    fn place(cwd: &Path, home: &Path) -> Place {
        Place {
            cwd: cwd.to_path_buf(),
            home: home.to_path_buf(),
            user_home: None,
            session: PathBuf::from("/s"),
            created: 1_789_464_757,
        }
    }

    #[test]
    fn instructions_scale_to_small_context_windows_and_are_stable() {
        let dir = tempfile::tempdir().expect("temp dir");
        let cwd = dir.path().join("project");
        let home = dir.path().join("home");
        write(&cwd.join("AGENTS.md"), &"project rules\n".repeat(10_000));
        for n in 0..100 {
            write(
                &cwd.join(format!(".agents/skills/skill-{n}/SKILL.md")),
                &format!(
                    "---\nname: skill-{n}\ndescription: {}\n---\nInstructions",
                    "description ".repeat(100)
                ),
            );
        }
        let place = place(&cwd, &home);
        let small = build(&place, 8000, &[]);
        let large = build(&place, 200_000, &[]);
        assert!(small.text.len() < 12_000, "{} bytes", small.text.len());
        assert!(large.text.len() > small.text.len());
        assert!(small.text.contains("[truncated]"));
        assert!(small.text.contains("- Session started: 2026-09-15 09:32 UTC;"));
        assert_eq!(
            small.text,
            build(&place, 8000, &[]).text,
            "unchanged files build the same instructions"
        );
    }

    #[test]
    fn server_catalogs_leave_out_tools_then_servers() {
        let listing = |n: usize| mcp::Listing {
            name: format!("server-{n}"),
            target: "npx server".into(),
            about: Some("Does <things> & more.".into()),
            tools: vec!["a".repeat(40); 10],
        };
        let catalog = server_catalog(&[listing(1)], CATALOG_BUDGET);
        assert!(
            catalog.contains("<description>Does &lt;things&gt; &amp; more.</description>"),
            "{catalog}"
        );
        assert!(catalog.contains("<tools>"), "{catalog}");
        let many: Vec<mcp::Listing> = (0..400).map(listing).collect();
        let catalog = server_catalog(&many, CATALOG_BUDGET);
        assert!(!catalog.contains("<tools>"), "tools go first");
        assert!(
            catalog.ends_with("more servers are set up but not listed; agt mcp list names them.")
        );
    }

    #[test]
    fn bounded_reads_distinguish_clipped_unicode_from_invalid_files() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("AGENTS.md");
        fs::write(&path, "é界end").expect("instructions");
        assert_eq!(read_prefix(&path, 3).expect("clipped"), ("é".into(), true));
        assert_eq!(read_prefix(&path, 8).expect("whole"), ("é界end".into(), false));
        fs::write(&path, [0xff, b'a']).expect("malformed");
        assert_eq!(
            read_prefix(&path, 1).expect_err("invalid UTF-8").kind(),
            io::ErrorKind::InvalidData
        );
    }
}
