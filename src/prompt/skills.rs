//! Agent Skills: finding the skills a session has, the catalog the
//! instructions carry, and the instructions a skill loads.
//!
//! agt follows the Agent Skills integration guide. The catalog names each
//! skill with its description and the location of its `SKILL.md`, which the
//! model reads with `cat` to load it, and `/name` or `$name` in a message
//! loads it for the user. Loaded instructions are wrapped in
//! `<skill_content>`, so compaction can find and keep them.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use super::{CATALOG_DESCRIPTION, fill, project_root, read_prefix, xml_escape};

/// Largest skill body given to the model at once (about 5k tokens).
const SKILL_LIMIT: usize = 20 * 1024;
/// Largest `SKILL.md` read while discovering skills.
const READ_LIMIT: usize = SKILL_LIMIT + 16 * 1024;
/// How skill instructions begin wherever they appear in the context.
pub(crate) const SKILL_CONTENT: &str = "<skill_content";

/// A skill from the catalog.
#[derive(Clone, Debug)]
pub(crate) struct Skill {
    pub(crate) name: String,
    pub(crate) description: String,
    /// Absolute path of the skill's `SKILL.md`.
    pub(crate) path: PathBuf,
}

/// The skills for `cwd`, and problems found loading them. Directories are
/// searched from `cwd` up to the project root, then agt's `home`, then the
/// user's home; the first skill of a name wins.
pub(super) fn discover(
    cwd: &Path,
    home: &Path,
    user_home: Option<&Path>,
) -> (Vec<Skill>, Vec<String>) {
    let mut skills: Vec<Skill> = Vec::new();
    let mut warnings = Vec::new();
    let mut visited = Vec::new();
    for root in roots(cwd, home, user_home) {
        // Symlinked roots, or a project root at the home directory, would
        // otherwise load and report the same skills twice.
        let Ok(canonical) = root.canonicalize() else {
            continue;
        };
        if visited.contains(&canonical) {
            continue;
        }
        visited.push(canonical);
        let Ok(entries) = fs::read_dir(&root) else {
            continue;
        };
        let mut dirs: Vec<PathBuf> = entries
            .filter_map(|entry| Some(entry.ok()?.path()))
            .filter(|path| path.join("SKILL.md").is_file())
            .collect();
        dirs.sort();
        for dir in dirs {
            let path = dir.join("SKILL.md");
            if skills.iter().any(|skill| skill.path == path) {
                continue;
            }
            match load(&dir, &path) {
                Ok((skill, problems)) => match skills.iter().find(|s| s.name == skill.name) {
                    // The same skill installed for several clients is no conflict.
                    Some(winner) if winner.description == skill.description => {}
                    Some(winner) => warnings.push(format!(
                        "skill {} at {} is shadowed by {}",
                        skill.name,
                        path.display(),
                        winner.path.display()
                    )),
                    None => {
                        warnings.extend(problems);
                        skills.push(skill);
                    }
                },
                Err(problem) => warnings.push(problem),
            }
        }
    }
    (skills, warnings)
}

/// Skill directories in precedence order: from `cwd` up to the project root,
/// then agt's own, then the user's. In each place `.agents/skills` is the
/// cross-client convention and `.claude/skills` is read for compatibility.
fn roots(cwd: &Path, home: &Path, user_home: Option<&Path>) -> Vec<PathBuf> {
    let top = project_root(cwd).unwrap_or(cwd);
    let mut roots = Vec::new();
    for dir in cwd.ancestors() {
        roots.push(dir.join(".agents/skills"));
        roots.push(dir.join(".claude/skills"));
        if dir == top {
            break;
        }
    }
    roots.push(home.join("skills"));
    if let Some(user_home) = user_home {
        roots.push(user_home.join(".agents/skills"));
        roots.push(user_home.join(".claude/skills"));
    }
    roots
}

/// Loads a skill leniently, as the Agent Skills integration guide recommends:
/// naming problems are warnings, and a missing description skips the skill.
fn load(dir: &Path, path: &Path) -> Result<(Skill, Vec<String>), String> {
    let skipped = |reason: &str| format!("skipped skill {}: {reason}", path.display());
    let (text, _) = read_prefix(path, READ_LIMIT).map_err(|error| skipped(&error.to_string()))?;
    let (fields, _) =
        split_frontmatter(&text).ok_or_else(|| skipped("SKILL.md has no YAML frontmatter"))?;
    let field = |key: &str| {
        fields
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.trim())
            .filter(|value| !value.is_empty())
    };
    let description = field("description").ok_or_else(|| skipped("missing description"))?;
    let dir_name =
        dir.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let mut warnings = Vec::new();
    let name = match field("name") {
        Some(name) => name.to_owned(),
        None => {
            warnings
                .push(format!("skill {} has no name; using its directory name", path.display()));
            dir_name.clone()
        }
    };
    // Loaded skills are recognized by the name quoted in their markers.
    if name.contains(['"', '<', '>', '&']) {
        return Err(skipped("the name contains quotes or markup"));
    }
    if !valid_name(&name) {
        warnings.push(format!(
            "skill name {name:?} in {} should be 1-64 lowercase letters, digits and single hyphens",
            path.display()
        ));
    }
    if name != dir_name {
        warnings
            .push(format!("skill name {name:?} does not match its directory {}", dir.display()));
    }
    if description.chars().count() > 1024 {
        warnings.push(format!("skill {name} has a description over 1024 characters"));
    }
    let skill = Skill { name, description: description.to_owned(), path: path.to_path_buf() };
    Ok((skill, warnings))
}

fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name.split('-').all(|part| {
            !part.is_empty() && part.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

/// The skills catalog within `budget` bytes, shortening descriptions and then
/// listing fewer skills when the whole catalog does not fit.
pub(super) fn catalog(skills: &[Skill], budget: usize) -> String {
    let entry = |skill: &Skill, description: &str| {
        format!(
            "\n<skill>\n<name>{}</name>\n<description>{}</description>\n<location>{}</location>\n</skill>",
            xml_escape(&skill.name),
            xml_escape(description),
            xml_escape(&skill.path.to_string_lossy()),
        )
    };
    let mut entries: Vec<String> =
        skills.iter().map(|skill| entry(skill, &skill.description)).collect();
    if entries.iter().map(String::len).sum::<usize>() > budget {
        entries = skills
            .iter()
            .map(|skill| {
                let description: String =
                    skill.description.chars().take(CATALOG_DESCRIPTION).collect();
                entry(skill, &description)
            })
            .collect();
    }
    let mut out = String::from(
        "\n\n# Skills\n\
         Skills are folders of instructions, scripts and resources for particular tasks. When a task matches a skill's description, read its SKILL.md with cat before you start, and resolve relative paths in it against the skill's folder.\n\n\
         <available_skills>",
    );
    let listed = fill(&mut out, &entries, budget);
    out.push_str("\n</available_skills>");
    if listed < skills.len() {
        let _ =
            write!(out, "\n{} more skills are installed but not listed.", skills.len() - listed);
    }
    out
}

/// A skill's instructions wrapped for the model, clipped to `SKILL_LIMIT`.
pub(crate) fn skill_block(skill: &Skill) -> std::io::Result<String> {
    let (text, truncated) = read_prefix(&skill.path, READ_LIMIT)?;
    let mut body =
        split_frontmatter(&text).map_or(text.as_str(), |(_, body)| body).trim().to_owned();
    if body.len() > SKILL_LIMIT || truncated {
        let end = body.floor_char_boundary(SKILL_LIMIT.min(body.len()));
        body.truncate(end);
        body.push_str("\n[…the rest is in SKILL.md]");
    }
    let dir = skill.path.parent().unwrap_or(&skill.path);
    Ok(format!(
        "{SKILL_CONTENT} name=\"{}\" path=\"{}\">\nSkill directory: {}\n\n{body}\n</skill_content>",
        xml_escape(&skill.name),
        xml_escape(&skill.path.to_string_lossy()),
        dir.display()
    ))
}

/// What the user wrote in a message, without the skill instructions agt
/// inlined after it.
pub(crate) fn user_words(text: &str) -> &str {
    text.find(SKILL_CONTENT).map_or(text, |start| &text[..start]).trim_end()
}

/// Names of the skills whose instructions appear in `text`.
pub(crate) fn skill_names(text: &str) -> impl Iterator<Item = &str> {
    text.split("<skill_content name=\"").skip(1).filter_map(|rest| rest.split('"').next())
}

/// How a frontmatter value is written.
#[derive(Clone, Copy, PartialEq)]
enum Scalar {
    Plain,
    Quoted(char),
    Literal,
    Folded,
}

/// A top-level value, which may continue on following lines.
struct Field<'a> {
    key: String,
    scalar: Scalar,
    lines: Vec<&'a str>,
}

impl Field<'_> {
    fn continues(&self, line: &str) -> bool {
        match self.scalar {
            Scalar::Literal | Scalar::Folded | Scalar::Plain => {
                line.starts_with([' ', '\t']) || line.trim().is_empty()
            }
            Scalar::Quoted(quote) => closing_quote(&self.lines.join(" "), quote).is_none(),
        }
    }

    fn finish(self) -> Option<(String, String)> {
        let plain = self.scalar == Scalar::Plain;
        let value = match self.scalar {
            Scalar::Literal => self.lines.join("\n").trim_end().to_owned(),
            // Lines fold into spaces and blank lines into newlines.
            Scalar::Folded | Scalar::Plain => {
                let folded = self
                    .lines
                    .split(|line| line.is_empty())
                    .map(|paragraph| {
                        let lines: Vec<&str> = paragraph
                            .iter()
                            .map(|line| if plain { strip_comment(line) } else { line })
                            .collect();
                        lines.join(" ")
                    })
                    .filter(|paragraph| !paragraph.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n");
                match folded.chars().next() {
                    // A quoted value can start on the line after its key.
                    Some(quote @ ('"' | '\'')) if plain => unquote(&folded, quote)?,
                    _ => folded,
                }
            }
            Scalar::Quoted(quote) => unquote(&self.lines.join(" "), quote)?,
        };
        Some((self.key, value))
    }
}

/// Splits a document into its frontmatter fields and body. Only top-level
/// scalars are read, which is all the catalog needs: plain values (also over
/// several indented lines), quoted strings, and `|` or `>` blocks. Nested
/// mappings such as `metadata` are skipped. A colon inside an unquoted value
/// is kept, which accepts a common mistake in hand-written skills.
fn split_frontmatter(text: &str) -> Option<(Vec<(String, String)>, &str)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let rest = text.strip_prefix("---\n").or_else(|| text.strip_prefix("---\r\n"))?;
    let mut fields = Vec::new();
    let mut open: Option<Field> = None;
    let mut offset = 0;
    for raw in rest.split_inclusive('\n') {
        offset += raw.len();
        let line = raw.trim_end_matches(['\n', '\r']);
        if line.trim_end() == "---" {
            if let Some(field) = open.take() {
                fields.push(field.finish()?);
            }
            return Some((fields, &rest[offset..]));
        }
        if open.as_ref().is_some_and(|field| field.continues(line)) {
            if let Some(field) = &mut open {
                field.lines.push(line.trim());
            }
            continue;
        }
        if let Some(field) = open.take() {
            fields.push(field.finish()?);
        }
        if line.starts_with([' ', '\t', '#']) || line.trim().is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        let scalar = if is_block(value, '|') {
            Scalar::Literal
        } else if is_block(value, '>') {
            Scalar::Folded
        } else if let Some(quote) = value.chars().next().filter(|c| matches!(c, '"' | '\'')) {
            Scalar::Quoted(quote)
        } else {
            Scalar::Plain
        };
        let lines = match scalar {
            Scalar::Literal | Scalar::Folded => Vec::new(),
            Scalar::Plain | Scalar::Quoted(_) => vec![value],
        };
        open = Some(Field { key: key.trim().to_owned(), scalar, lines });
    }
    None
}

/// Whether `value` starts a block scalar, such as `|`, `>-` or `|2`.
fn is_block(value: &str, indicator: char) -> bool {
    value
        .strip_prefix(indicator)
        .is_some_and(|rest| strip_comment(rest).chars().all(|c| matches!(c, '+' | '-' | '0'..='9')))
}

fn strip_comment(value: &str) -> &str {
    value.find(" #").map_or(value, |start| &value[..start]).trim_end()
}

/// The byte index of the quote closing a value that begins with `quote`.
fn closing_quote(text: &str, quote: char) -> Option<usize> {
    let mut chars = text.char_indices().skip(1).peekable();
    while let Some((index, c)) = chars.next() {
        if c == '\\' && quote == '"' {
            chars.next();
        } else if c == quote {
            // Inside single quotes, a doubled quote stands for one quote.
            if quote == '\'' && chars.next_if(|(_, next)| *next == '\'').is_some() {
                continue;
            }
            return Some(index);
        }
    }
    None
}

/// The text of a quoted value, ignoring anything after its closing quote,
/// such as a comment.
fn unquote(text: &str, quote: char) -> Option<String> {
    let end = closing_quote(text, quote)?;
    let inner = text.get(1..end).unwrap_or_default();
    if quote == '\'' {
        return Some(inner.replace("''", "'"));
    }
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('0') => out.push('\0'),
            Some('a') => out.push('\u{7}'),
            Some('b') => out.push('\u{8}'),
            Some('v') => out.push('\u{b}'),
            Some('f') => out.push('\u{c}'),
            Some('e') => out.push('\u{1b}'),
            Some('N') => out.push('\u{85}'),
            Some('_') => out.push('\u{a0}'),
            Some('L') => out.push('\u{2028}'),
            Some('P') => out.push('\u{2029}'),
            Some(escape @ ('x' | 'u' | 'U')) => {
                let count = match escape {
                    'x' => 2,
                    'u' => 4,
                    _ => 8,
                };
                let mut code = 0;
                for _ in 0..count {
                    code = code * 16 + chars.next()?.to_digit(16)?;
                }
                out.push(char::from_u32(code)?);
            }
            Some(escaped @ ('\\' | '"' | '/' | ' ')) => out.push(escaped),
            _ => return None,
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        fs::write(path, text).expect("write");
    }

    #[test]
    fn frontmatter_reads_the_scalars_skills_use() {
        let doc = "---\nname: pdf-tools\ndescription: >-\n  Extract text\n  from PDFs.\n\n  Use for forms: fill them.\nlicense: 'Apache-2.0'\nmetadata:\n  author: someone\ncompatibility: \"needs \\\"qpdf\\\" and a \\\\n literal\"\nnotes: |2\n  line one\n  line two\nsummary: Handles PDFs,\n  forms and scans. # trailing comment\nquoted: \"spans\n  two lines\"\ncommented: \"Deploy things\" # main\nescaped: 'it''s' # note\nwhen: Use when: the user asks\n---\n# Body\n";
        let (fields, body) = split_frontmatter(doc).expect("frontmatter");
        let get = |key: &str| fields.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
        assert_eq!(get("name"), Some("pdf-tools"));
        assert_eq!(get("description"), Some("Extract text from PDFs.\nUse for forms: fill them."));
        assert_eq!(get("license"), Some("Apache-2.0"));
        assert_eq!(get("compatibility"), Some("needs \"qpdf\" and a \\n literal"));
        assert_eq!(get("notes"), Some("line one\nline two"));
        assert_eq!(get("summary"), Some("Handles PDFs, forms and scans."));
        assert_eq!(get("quoted"), Some("spans two lines"));
        assert_eq!(get("commented"), Some("Deploy things"));
        assert_eq!(get("escaped"), Some("it's"));
        assert_eq!(get("when"), Some("Use when: the user asks"));
        assert_eq!(get("author"), None, "nested mappings are skipped");
        assert_eq!(body, "# Body\n");
        let (fields, _) =
            split_frontmatter("---\nname: x\ndescription: \"Caf\\u00e9 \\U0001f600\"\n---\nbody")
                .expect("escaped Unicode");
        assert_eq!(fields[1].1, "Café 😀");
        let (fields, _) = split_frontmatter(
            "---\ndescription:\n  \"Deploy things\"\nsummary: First\n  line.\n\n  Second.\nname: x\n---\n",
        )
        .expect("frontmatter");
        assert_eq!(fields[0].1, "Deploy things");
        assert_eq!(fields[1].1, "First line.\nSecond.");
        assert_eq!(fields[2].1, "x");
    }

    #[test]
    fn malformed_frontmatter_is_not_read() {
        for doc in [
            "# no frontmatter",
            "---\nname: x\n",
            "---\nname: x\ndescription: \"unterminated\n---\nbody",
            "---\nname: x\ndescription: \"bad\\uXYZW\"\n---\nbody",
        ] {
            let read = split_frontmatter(doc);
            assert!(read.is_none(), "{doc:?} was read as {:?}", read.map(|(fields, _)| fields));
        }
    }

    #[test]
    fn skills_are_discovered_with_precedence_and_lenient_validation() {
        let root = tempfile::tempdir().expect("temp dir");
        let project = root.path().join("project");
        let home = root.path().join("home");
        let agt_home = root.path().join("agt");
        fs::create_dir_all(project.join(".git")).expect("git dir");
        let cwd = project.join("crates/core");
        fs::create_dir_all(&cwd).expect("cwd");
        write(
            &cwd.join(".agents/skills/deploy/SKILL.md"),
            "---\nname: deploy\ndescription: Deploy from the crate.\n---\nsteps",
        );
        write(
            &project.join(".claude/skills/deploy/SKILL.md"),
            "---\nname: deploy\ndescription: Shadowed.\n---\n",
        );
        write(&agt_home.join("skills/release/SKILL.md"), "---\ndescription: Cut a release.\n---\n");
        write(
            &home.join(".agents/skills/Review_Code/SKILL.md"),
            "---\nname: Review_Code\ndescription: Review <diffs> & more\n---\n",
        );
        write(&home.join(".agents/skills/empty/SKILL.md"), "---\nname: empty\n---\n");
        let copy = "---\nname: lint\ndescription: Lint the code.\n---\n";
        write(&home.join(".agents/skills/lint/SKILL.md"), copy);
        write(&home.join(".claude/skills/lint/SKILL.md"), copy);
        write(
            &home.join(".claude/skills/quoted/SKILL.md"),
            "---\nname: say \"hi\"\ndescription: Greets.\n---\n",
        );

        let (skills, warnings) = discover(&cwd, &agt_home, Some(&home));
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["deploy", "release", "Review_Code", "lint"]);
        let shadowed = warnings.iter().filter(|w| w.contains("shadowed")).count();
        assert_eq!(shadowed, 1, "identical copies are not reported: {warnings:?}");
        for problem in ["has no name", "should be 1-64", "missing description", "quotes or markup"]
        {
            let warned = warnings.iter().any(|warning| warning.contains(problem));
            assert!(warned, "no warning says {problem:?}: {warnings:?}");
        }
        let (_, again) = discover(&cwd, &agt_home, Some(&project));
        let named = again.iter().filter(|w| w.contains("has no name")).count();
        assert_eq!(named, 1, "a root reached twice is read once: {again:?}");
        assert!(!valid_name("a--b") && !valid_name("A") && valid_name("pdf-processing"));
    }

    #[test]
    fn loaded_skills_are_wrapped_with_their_directory_and_clipped() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("deploy/SKILL.md");
        write(&path, "---\nname: deploy\ndescription: Deploy.\n---\nsteps");
        let skill = Skill { name: "deploy".into(), description: "Deploy.".into(), path };
        let block = skill_block(&skill).expect("block");
        let expected = format!(
            "<skill_content name=\"deploy\" path=\"{}\">\nSkill directory: {}\n\nsteps\n</skill_content>",
            skill.path.display(),
            dir.path().join("deploy").display()
        );
        assert_eq!(block, expected);
        assert_eq!(skill_names(&format!("x {block} y")).collect::<Vec<_>>(), ["deploy"]);
        assert_eq!(user_words(&format!("fix it\n\n{block}")), "fix it");

        let long = "step\n".repeat(SKILL_LIMIT);
        write(&skill.path, &format!("---\nname: deploy\ndescription: Deploy.\n---\n{long}"));
        let block = skill_block(&skill).expect("block");
        let end: Vec<&str> = block.lines().rev().take(2).collect();
        assert_eq!(end, ["</skill_content>", "[…the rest is in SKILL.md]"]);
    }

    #[test]
    fn catalogs_escape_markup_and_fit_their_budget() {
        let skill = Skill {
            name: "review".into(),
            description: "Review <diffs> & \"more\"".into(),
            path: PathBuf::from("/skills/review/SKILL.md"),
        };
        let catalog = catalog(&[skill], 16 * 1024);
        let escaped = "<description>Review &lt;diffs&gt; &amp; &quot;more&quot;</description>";
        assert!(catalog.contains(escaped), "{catalog}");

        let skills: Vec<Skill> = (0..200)
            .map(|index| Skill {
                name: format!("skill-{index}"),
                description: "d".repeat(900),
                path: PathBuf::from(format!("/skills/skill-{index}/SKILL.md")),
            })
            .collect();
        let catalog = super::catalog(&skills, 16 * 1024);
        assert!(catalog.len() < 16 * 1024 + 1024);
        assert!(!catalog.contains(&"d".repeat(CATALOG_DESCRIPTION + 1)));
        assert!(catalog.ends_with("more skills are installed but not listed."));
    }
}
