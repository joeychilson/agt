//! Completing the input: commands and skills after a leading `/`, and files to
//! mention after `@`, from the working directory's files, which are listed on
//! a thread the first time they are wanted.

use std::collections::VecDeque;
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;

use super::picker::{Choice, Item, Picker};
use super::{App, COMMANDS, Input};
use crate::agent::{Agent, Delivery};

/// The most files listed.
const MAX_FILES: usize = 50_000;
/// Directories a walk outside a repository skips, besides hidden ones.
const SKIPPED: [&str; 2] = ["node_modules", "target"];

/// What a completion list completes.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Completing {
    /// A command or skill after `/`.
    Command,
    /// A file mentioned after `@`.
    File,
}

/// A list completing the text before the cursor.
pub(super) struct Completion {
    pub(super) completing: Completing,
    pub(super) picker: Picker,
    /// The text a choice replaces.
    range: Range<usize>,
    /// Whether the list holds its choices, rather than waiting for the files.
    pub(super) listed: bool,
}

impl Completion {
    /// The commands, then the skills of `agent`, each matched by its name.
    fn commands(agent: Option<&Agent>) -> Self {
        let commands = COMMANDS.iter().map(|(_, name, about)| {
            Item::new(*name).cell(*about).matched_by(&name[1..]).group("Commands")
        });
        let skills = agent.map_or(&[][..], Agent::skills).iter().map(|skill| {
            Item::new(format!("/{}", skill.name))
                .cell(skill.description.as_str())
                .matched_by(&skill.name)
                .group("Skills")
        });
        let picker = Picker::new(commands.chain(skills).collect());
        Self { completing: Completing::Command, picker, range: 0..0, listed: true }
    }

    /// The working directory's files, or an empty list waiting for them.
    fn files(files: Option<&[String]>) -> Self {
        let items = files.unwrap_or_default().iter().map(|path| Item::new(path.as_str()));
        let picker = Picker::new(items.collect());
        Self { completing: Completing::File, picker, range: 0..0, listed: files.is_some() }
    }
}

impl App {
    /// Opens the list that completes what the input holds before the cursor,
    /// or closes it when nothing there is completed.
    pub(super) fn refresh_completion(&mut self) {
        let text = self.editor.text();
        if self.menu.is_some() || self.dismissed.as_deref() == Some(text) {
            self.completion = None;
            return;
        }
        self.dismissed = None;
        if let Some(name) =
            text.strip_prefix('/').filter(|name| !name.contains(char::is_whitespace))
        {
            let mut completion = match self.completion.take() {
                Some(completion) if completion.completing == Completing::Command => completion,
                _ => Completion::commands(self.agent.as_ref()),
            };
            completion.picker.set_query(name);
            completion.range = 0..text.len();
            self.completion = Some(completion);
            return;
        }
        let cursor = self.editor.cursor();
        let Some((start, query)) = mention(text, cursor) else {
            self.completion = None;
            return;
        };
        if self.files.is_none() && !self.listing_files {
            self.listing_files = true;
            if !list_on_thread(self.sender.clone(), self.cwd.clone()) {
                self.files = Some(Vec::new());
            }
        }
        let listed = self.files.is_some();
        let mut completion = match self.completion.take() {
            Some(completion)
                if completion.completing == Completing::File && completion.listed == listed =>
            {
                completion
            }
            _ => Completion::files(self.files.as_deref()),
        };
        completion.picker.set_query(query);
        completion.range = start..cursor;
        self.completion = Some(completion);
    }

    /// Completes the input to the selected choice. With `run`, a command the
    /// input starts, or a skill it names, runs instead. Returns false when
    /// nothing is selected.
    pub(super) fn accept_completion(&mut self, run: bool) -> bool {
        let Some(completion) = &self.completion else {
            return false;
        };
        let Some(Choice::Item(index)) = completion.picker.choice() else {
            return false;
        };
        let label = completion.picker.label(index).to_owned();
        match completion.completing {
            Completing::File => {
                let range = completion.range.clone();
                self.completion = None;
                self.editor.replace(range, &format!("{label} "));
            }
            Completing::Command => {
                let text = self.editor.text();
                // Commands come before skills in the list. A loose match is
                // only completed, never run.
                if run && (label == text || (index < COMMANDS.len() && label.starts_with(text))) {
                    self.editor.set(label);
                    self.submit(Delivery::Next);
                } else {
                    self.editor.set(format!("{label} "));
                }
            }
        }
        true
    }
}

/// Lists the files under `cwd` on a thread, which reports them to the event
/// loop. Returns false when the thread cannot start.
fn list_on_thread(sender: mpsc::Sender<Input>, cwd: PathBuf) -> bool {
    let spawned = thread::Builder::new().name("agt-files".into()).spawn(move || {
        let _ = sender.send(Input::Files(list(&cwd)));
    });
    spawned.is_ok()
}

/// The `@` mention the cursor is in: where its `@` starts and what follows it.
fn mention(text: &str, cursor: usize) -> Option<(usize, &str)> {
    let before = text.get(..cursor)?;
    let start = before
        .rfind(char::is_whitespace)
        .map_or(0, |index| index + before[index..].chars().next().map_or(1, char::len_utf8));
    before[start..].strip_prefix('@').map(|query| (start, query))
}

/// The files under `cwd`, relative to it. In a repository these are the files
/// git tracks or would track, so ignored ones stay out; elsewhere a walk finds
/// them, nearest first.
fn list(cwd: &Path) -> Vec<String> {
    let git = Command::new("git")
        .args(["ls-files", "--cached", "--others", "--exclude-standard", "-z"])
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match git {
        Ok(output) if output.status.success() => output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .take(MAX_FILES)
            .map(|path| String::from_utf8_lossy(path).into_owned())
            .collect(),
        _ => walk(cwd),
    }
}

fn walk(cwd: &Path) -> Vec<String> {
    let mut files = Vec::new();
    let mut dirs = VecDeque::from([PathBuf::new()]);
    while let Some(dir) = dirs.pop_front() {
        let Ok(entries) = fs::read_dir(cwd.join(&dir)) else { continue };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            let name = entry.file_name();
            let Some(name) = name.to_str().filter(|name| !name.starts_with('.')) else {
                continue;
            };
            let path = dir.join(name);
            match entry.file_type() {
                Ok(kind) if kind.is_dir() && !SKIPPED.contains(&name) => dirs.push_back(path),
                Ok(kind) if kind.is_file() => {
                    files.push(path.to_string_lossy().into_owned());
                    if files.len() == MAX_FILES {
                        return files;
                    }
                }
                _ => {}
            }
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::super::test_app;
    use super::super::text::plain;
    use super::*;

    #[test]
    fn commands_complete_by_name_under_their_headings() {
        let (mut app, _dir) = test_app((80, 24));
        app.editor.insert("/");
        app.refresh_completion();
        let completion = app.completion.as_mut().expect("a completion list");
        let rows: Vec<String> = completion
            .picker
            .show(40, 3)
            .rows
            .iter()
            .map(|row| plain(row).trim_end().to_owned())
            .collect();
        assert_eq!(rows[0], " Commands");
        assert!(rows[1].starts_with(" /login"), "{rows:?}");
        app.editor.insert("mo");
        app.refresh_completion();
        assert!(app.accept_completion(false));
        assert_eq!(app.editor.text(), "/model ");
    }

    #[test]
    fn mentions_are_found_before_the_cursor() {
        assert_eq!(mention("read @src/tu", 12), Some((5, "src/tu")));
        assert_eq!(mention("@", 1), Some((0, "")));
        assert_eq!(mention("mail a@b", 8), None);
        assert_eq!(mention("read @src now", 13), None);
        assert_eq!(mention("界 @x", "界 @x".len()), Some(("界 ".len(), "x")));
    }

    #[test]
    fn a_walk_lists_nearest_files_first_and_skips_hidden_and_build_directories() {
        let dir = tempfile::tempdir().expect("temp dir");
        for path in ["b.rs", "src/a.rs", ".git/config", "target/debug/agt", "node_modules/x.js"] {
            let path = dir.path().join(path);
            fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
            fs::write(path, "x").expect("file");
        }
        assert_eq!(walk(dir.path()), ["b.rs", "src/a.rs"]);
    }

    #[test]
    fn a_repository_lists_what_git_would_track() {
        let dir = tempfile::tempdir().expect("temp dir");
        let git = |args: &[&str]| {
            Command::new("git").args(args).current_dir(dir.path()).output().expect("git runs")
        };
        git(&["init", "-q"]);
        fs::write(dir.path().join(".gitignore"), "out.log\n").expect("ignore");
        fs::write(dir.path().join("kept.rs"), "x").expect("file");
        fs::write(dir.path().join("out.log"), "x").expect("file");
        let mut files = list(dir.path());
        files.sort();
        assert_eq!(files, [".gitignore", "kept.rs"]);
    }
}
