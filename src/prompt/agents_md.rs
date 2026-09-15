//! AGENTS.md files: those that apply to the working directory, whose text the
//! instructions carry, and those elsewhere in the repository, which they list.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::{project_root, read_prefix, xml_escape};

/// Most AGENTS.md text the instructions carry.
const BUDGET: usize = 32 * 1024;
/// Most AGENTS.md files elsewhere in the repository listed by path.
const NESTED_LIMIT: usize = 50;

/// The AGENTS.md section of the instructions for `cwd`, carrying at most
/// `budget` bytes of instructions, or nothing when no file applies or exists.
pub(super) fn section(cwd: &Path, home: &Path, budget: usize) -> String {
    let files = applicable(cwd, home, budget.min(BUDGET));
    let (nested, total) = nested(cwd);
    if files.is_empty() && total == 0 {
        return String::new();
    }
    let mut out = String::from(
        "\n\n# AGENTS.md\n\
         Project instructions from AGENTS.md files, most general first. Each applies to the directory tree under its folder; more specific files take precedence, and the user's messages override them all.",
    );
    for (path, text) in files {
        let _ = write!(
            out,
            "\n\n<agents_md path=\"{}\">\n{}\n</agents_md>",
            xml_escape(&path.to_string_lossy()),
            text.trim()
        );
    }
    if total > 0 {
        out.push_str(
            "\n\nThese AGENTS.md files cover other parts of the repository. Before editing a file, read every applicable instruction file along its directory path, from general to specific:",
        );
        for path in &nested {
            let _ = write!(out, "\n- {}", path.display());
        }
        if total > nested.len() {
            let _ =
                write!(out, "\n- and {} more (git ls-files '*AGENTS.md')", total - nested.len());
        }
    }
    out
}

/// AGENTS.md files that apply to `cwd`, most general first: agt's global
/// file, then one per directory from the project root down to `cwd`.
/// `CLAUDE.md` stands in where a directory has no AGENTS.md. The budget is
/// spent on the most specific files first, since they take precedence.
fn applicable(cwd: &Path, home: &Path, mut budget: usize) -> Vec<(PathBuf, String)> {
    let dirs: Vec<&Path> = match project_root(cwd) {
        Some(root) => cwd.ancestors().take_while(|dir| *dir != root).chain([root]).collect(),
        None => vec![cwd],
    };
    let candidates = dirs
        .into_iter()
        .filter_map(|dir| {
            ["AGENTS.md", "CLAUDE.md"]
                .into_iter()
                .map(|name| dir.join(name))
                .find(|path| path.is_file())
        })
        .chain([home.join("AGENTS.md")]);
    let mut files = Vec::new();
    for path in candidates {
        if budget == 0 {
            break;
        }
        let Ok((mut text, truncated)) = read_prefix(&path, budget) else {
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        if truncated {
            text.push_str("\n[truncated]");
        }
        budget = budget.saturating_sub(text.len());
        files.push((path, text));
    }
    files.reverse();
    files
}

/// AGENTS.md files elsewhere in the repository, which do not apply to `cwd`
/// itself, with how many there are in total.
fn nested(cwd: &Path) -> (Vec<PathBuf>, usize) {
    let Some(root) = project_root(cwd) else {
        return (Vec::new(), 0);
    };
    let output = Command::new("git")
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            ":(glob)**/AGENTS.md",
        ])
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let Some(output) = output.ok().filter(|output| output.status.success()) else {
        return (Vec::new(), 0);
    };
    // NUL-separated output keeps non-ASCII paths unquoted.
    let paths: Vec<PathBuf> = String::from_utf8_lossy(&output.stdout)
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(|path| root.join(path))
        .filter(|path| path.parent().is_none_or(|dir| !cwd.starts_with(dir)))
        .collect();
    let total = paths.len();
    (paths.into_iter().take(NESTED_LIMIT).collect(), total)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        fs::write(path, text).expect("write");
    }

    #[test]
    fn files_run_general_to_specific_and_budget_the_specific_first() {
        let root = tempfile::tempdir().expect("temp dir");
        let project = root.path().join("project");
        let agt_home = root.path().join("agt");
        fs::create_dir_all(project.join(".git")).expect("git dir");
        let cwd = project.join("app");
        write(&agt_home.join("AGENTS.md"), "global");
        write(&project.join("AGENTS.md"), "root");
        write(&project.join("CLAUDE.md"), "ignored because AGENTS.md exists");
        write(&cwd.join("CLAUDE.md"), "app");
        write(&root.path().join("AGENTS.md"), "outside the project");

        let texts = |budget| -> Vec<String> {
            applicable(&cwd, &agt_home, budget).into_iter().map(|(_, text)| text).collect()
        };
        assert_eq!(texts(BUDGET), ["global", "root", "app"]);

        write(&project.join("AGENTS.md"), &"r".repeat(BUDGET));
        let texts = texts(BUDGET);
        assert_eq!(texts.len(), 2, "the global file no longer fits");
        assert!(texts[0].ends_with("[truncated]"));
        assert_eq!(texts[1], "app");
    }

    #[test]
    fn nested_files_are_listed_from_the_repository_root() {
        let root = tempfile::tempdir().expect("temp dir");
        let repo = root.path();
        let status =
            Command::new("git").args(["init", "-q"]).current_dir(repo).status().expect("git runs");
        assert!(status.success());
        for path in
            ["AGENTS.md", "app/AGENTS.md", "app/sub/AGENTS.md", "café/AGENTS.md", "lib/AGENTS.md"]
        {
            write(&repo.join(path), "rules");
        }
        let (nested, total) = nested(&repo.join("app"));
        assert_eq!(total, 3);
        assert_eq!(
            nested,
            [
                repo.join("app/sub/AGENTS.md"),
                repo.join("café/AGENTS.md"),
                repo.join("lib/AGENTS.md")
            ]
        );
    }
}
