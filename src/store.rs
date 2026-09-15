//! Sessions on disk.
//!
//! Each session owns a directory, `$AGT_HOME/sessions/<id>/`, of plain files
//! that people and agents read with ordinary tools: `log.jsonl`, every record
//! of the session in order; `procs/<id>.log`, each process's output as text;
//! `images/`, the images the model saw; and `notes.md`, the agent's own notes.
//!
//! The log is only ever appended to. A compaction appends a record holding the
//! whole context it leaves, so resuming reads the log only from the last one,
//! and memory stays bounded by the context window however long a session runs.

use std::borrow::Cow;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use serde_json::error::Category;

use crate::agent::{self, Origin, Stop};
use crate::bash::Exit;
use crate::item::{Item, Kind};

/// The file in a session's directory that holds its log.
pub(crate) const LOG: &str = "log.jsonl";
/// The version of the log's format, which its first record names.
const VERSION: u32 = 1;
/// How every compaction record begins, so the last one can be found from the
/// end of the log. JSON strings escape newlines, so this can only follow a
/// newline at the start of a record.
const COMPACTION: &[u8] = br#"{"type":"compaction","#;
/// Bytes read at a time while searching back for a compaction record.
const SEARCH_CHUNK: u64 = 1024 * 1024;
/// The longest a session title is, in characters.
const TITLE_CHARS: usize = 72;

/// A line of the log. Records borrow what they write and own what they read.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Record<'a> {
    /// The first record: the format of the log and where the session began.
    Session { version: u32, cwd: Cow<'a, str>, created: u64 },
    /// Items after this come from `model` on `provider`, so a resumed session
    /// knows whose reasoning it may send back.
    Model { provider: Cow<'a, str>, model: Cow<'a, str> },
    /// An item of the conversation, as the model received or wrote it.
    Item {
        at: u64,
        /// Where a message came from, written only when not from the user.
        #[serde(default, skip_serializing_if = "Origin::is_user")]
        origin: Origin,
        item: Cow<'a, Item>,
    },
    /// A command started as process `id`.
    Proc { at: u64, id: u32, command: Cow<'a, str> },
    /// Process `id` exited.
    Exit { at: u64, id: u32, exit: Exit },
    /// Something agt told the user, such as a retry.
    Notice { at: u64, text: Cow<'a, str> },
    /// Something that went wrong.
    Error { at: u64, text: Cow<'a, str> },
    /// A turn ended.
    Turn { at: u64, stop: Stop },
    /// What a response cost, in dollars.
    Cost { usd: f64 },
    /// Old tool output was elided, keeping the newest `keep` bytes, so a
    /// resumed session elides it at the same point.
    Mask { keep: usize },
    /// The context a compaction left.
    Compaction(Snapshot<'a>),
    /// A record of a kind this agt does not know.
    #[serde(other)]
    Other,
}

/// The whole context a compaction leaves, from which a session resumes.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Snapshot<'a> {
    pub(crate) history: Cow<'a, [Item]>,
    /// The latest summary, which the next one updates.
    pub(crate) summary: Cow<'a, str>,
    /// User messages compaction took out of the context.
    pub(crate) users: Cow<'a, [String]>,
    /// The provider and model in use; their reasoning in `history` is sent back
    /// from `reasoning_from` on.
    pub(crate) provider: Cow<'a, str>,
    pub(crate) model: Cow<'a, str>,
    pub(crate) reasoning_from: usize,
    /// Dollars spent so far.
    pub(crate) cost: f64,
}

/// The time now, as records hold it: milliseconds since the Unix epoch.
pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
}

/// A time records hold as a date and minute in UTC, such as
/// `2026-09-15 14:02 UTC`.
pub(crate) fn timestamp(at: u64) -> String {
    let minutes = at / 60_000;
    let days = i64::try_from(minutes / 1440).unwrap_or(0);
    // Howard Hinnant's days-to-civil algorithm, over eras of 400 years that
    // begin on March 1st, so a leap day ends each year.
    let from_march = days + 719_468;
    let era = from_march.div_euclid(146_097);
    let day_of_era = from_march.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted + 2) / 5 + 1;
    let month = if shifted < 10 { shifted + 3 } else { shifted - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year}-{month:02}-{day:02} {:02}:{:02} UTC", minutes / 60 % 24, minutes % 60)
}

/// A session's log, open for appending.
pub(crate) struct Session {
    pub(crate) id: String,
    pub(crate) dir: PathBuf,
    /// Where the session runs now.
    pub(crate) cwd: PathBuf,
    /// When the session was created, in seconds since the Unix epoch.
    pub(crate) created: u64,
    log: File,
    /// Set after a failed write, which may have left a partial line.
    torn: bool,
}

/// The context a resumed session continues with.
#[derive(Default)]
pub(crate) struct Restored {
    pub(crate) history: Vec<Item>,
    pub(crate) summary: String,
    pub(crate) users: Vec<String>,
    pub(crate) model: Option<ModelUse>,
    /// Dollars spent over the whole session.
    pub(crate) cost: f64,
    /// Processes that started after the last compaction and never exited:
    /// they stopped with the agt that ran them.
    pub(crate) lost: Vec<(u32, String)>,
}

/// The provider and model a session last used, from which history index on.
#[derive(Debug, PartialEq)]
pub(crate) struct ModelUse {
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) since: usize,
}

impl Session {
    pub(crate) fn create(home: &Path, cwd: &Path) -> io::Result<Self> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        // Sessions started in the same instant, even under different homes,
        // must differ, since their control sockets share one directory.
        let mut random = [0; 4];
        SystemRandom::new()
            .fill(&mut random)
            .map_err(|_| io::Error::other("no secure random numbers are available"))?;
        let id = format!("{}-{:08x}", now.as_secs(), u32::from_be_bytes(random));
        let dir = home.join("sessions").join(&id);
        fs::create_dir_all(&dir)?;
        let log = File::options().append(true).create_new(true).open(dir.join(LOG))?;
        let created = now.as_secs();
        let mut session = Self { id, dir, cwd: cwd.to_path_buf(), created, log, torn: false };
        let cwd = cwd.to_string_lossy();
        session.write(&Record::Session { version: VERSION, cwd, created })?;
        Ok(session)
    }

    /// Opens session `id` to continue in `cwd` and returns its live context.
    /// With `replay`, every record ever logged is passed to it in order, which
    /// reads the whole log; otherwise reading starts at the last compaction.
    pub(crate) fn resume(
        home: &Path,
        id: &str,
        cwd: &Path,
        replay: Option<&mut dyn FnMut(&Record<'_>)>,
    ) -> io::Result<(Self, Restored)> {
        let saved = Saved::open(home, id)?;
        let len = saved.file.metadata()?.len();
        let restored = match replay {
            Some(replay) => saved.restore(saved.body, false, Some(replay))?,
            None => match last_compaction(&saved.file, saved.body, len)? {
                // A damaged record there is read around by reading everything.
                Some(offset) => saved
                    .restore(offset, true, None)
                    .or_else(|_| saved.restore(saved.body, false, None))?,
                None => saved.restore(saved.body, false, None)?,
            },
        };
        let path = saved.dir.join(LOG);
        let mut log = File::options().append(true).open(&path)?;
        let mut last = [0];
        if len > 0 && saved.file.read_at(&mut last, len - 1)? == 1 && last[0] != b'\n' {
            log.write_all(b"\n")?;
        }
        let Saved { dir, created, .. } = saved;
        let session =
            Self { id: id.to_owned(), dir, cwd: cwd.to_path_buf(), created, log, torn: false };
        Ok((session, restored))
    }

    pub(crate) fn log_path(&self) -> PathBuf {
        self.dir.join(LOG)
    }

    /// Appends `record` as a line. A failed write can leave a partial line
    /// behind, so the next write starts a fresh line to keep later records
    /// readable.
    pub(crate) fn write(&mut self, record: &Record<'_>) -> io::Result<()> {
        let mut line = serde_json::to_vec(record).map_err(io::Error::other)?;
        line.push(b'\n');
        if self.torn {
            self.log.write_all(b"\n")?;
            self.torn = false;
        }
        let result = self.log.write_all(&line);
        self.torn = result.is_err();
        result
    }
}

/// A session's log, opened only to read it, so a session that is running
/// meanwhile goes on undisturbed.
pub(crate) struct Saved {
    file: File,
    /// Where the records after the header begin.
    body: u64,
    /// The session's directory.
    pub(crate) dir: PathBuf,
    /// The directory the session began in.
    pub(crate) cwd: PathBuf,
    created: u64,
}

impl Saved {
    pub(crate) fn open(home: &Path, id: &str) -> io::Result<Self> {
        Self::at(session_dir(home, id)?)
    }

    /// Opens the log of the session kept in `dir`, checking its header.
    fn at(dir: PathBuf) -> io::Result<Self> {
        let file = File::open(dir.join(LOG))?;
        let mut line = Vec::new();
        BufReader::new(&file).take(64 * 1024).read_until(b'\n', &mut line)?;
        let Ok(Record::Session { version: VERSION, cwd, created }) = serde_json::from_slice(&line)
        else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the session log has no valid header",
            ));
        };
        let cwd = PathBuf::from(cwd.as_ref());
        Ok(Self { body: line.len() as u64, file, dir, cwd, created })
    }

    /// Passes each complete record from offset `from` on to `each`, in order,
    /// and returns the offset after the last one: where to read from when
    /// more is written. `from` 0 starts after the header.
    pub(crate) fn read(&self, from: u64, each: &mut dyn FnMut(Record<'static>)) -> io::Result<u64> {
        let mut offset = from.max(self.body);
        let mut reader = BufReader::new(&self.file);
        reader.seek(SeekFrom::Start(offset))?;
        let mut line = Vec::new();
        loop {
            line.clear();
            let read = reader.read_until(b'\n', &mut line)?;
            if read == 0 || line.last() != Some(&b'\n') {
                return Ok(offset);
            }
            offset += read as u64;
            // A line that is not a record, as a crash can leave, is passed over.
            if let Ok(record) = serde_json::from_slice(&line) {
                each(record);
            }
        }
    }

    /// Rebuilds the context from the records starting at `offset`, where a
    /// compaction record is expected first when resuming from one.
    fn restore(
        &self,
        offset: u64,
        compaction: bool,
        mut replay: Option<&mut dyn FnMut(&Record<'_>)>,
    ) -> io::Result<Restored> {
        let mut reader = BufReader::new(&self.file);
        reader.seek(SeekFrom::Start(offset))?;
        let mut restored = Restored::default();
        for (number, line) in reader.split(b'\n').enumerate() {
            let line = line?;
            // Line numbers count from the header; a read from a compaction that
            // fails is repeated from the header, which reports them correctly.
            let invalid = |problem: &str| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("session log line {}: {problem}", number + 2),
                )
            };
            let record = serde_json::from_slice::<Record<'static>>(&line);
            if number == 0 && compaction && !matches!(record, Ok(Record::Compaction(_))) {
                return Err(invalid("expected a compaction record"));
            }
            let record = match record {
                Ok(record) => record,
                // A crash can leave a torn final line; it is skipped.
                Err(error) if error.classify() != Category::Data => continue,
                Err(error) => return Err(invalid(&error.to_string())),
            };
            if let Some(replay) = &mut replay {
                replay(&record);
            }
            match record {
                Record::Session { .. } => return Err(invalid("a second session header")),
                Record::Item { item, .. } => restored.history.push(item.into_owned()),
                Record::Model { provider, model } => {
                    let since = restored.history.len();
                    let (provider, model) = (provider.into_owned(), model.into_owned());
                    restored.model = Some(ModelUse { provider, model, since });
                }
                Record::Proc { id, command, .. } => restored.lost.push((id, command.into_owned())),
                Record::Exit { id, .. } => restored.lost.retain(|(lost, _)| *lost != id),
                Record::Cost { usd } => restored.cost += usd,
                Record::Mask { keep } => {
                    agent::mask(&mut restored.history, keep);
                }
                Record::Compaction(snapshot) => {
                    let since = snapshot.reasoning_from;
                    let (provider, model) =
                        (snapshot.provider.into_owned(), snapshot.model.into_owned());
                    restored.model = Some(ModelUse { provider, model, since });
                    restored.history = snapshot.history.into_owned();
                    restored.summary = snapshot.summary.into_owned();
                    restored.users = snapshot.users.into_owned();
                    restored.cost = snapshot.cost;
                }
                Record::Notice { .. }
                | Record::Error { .. }
                | Record::Turn { .. }
                | Record::Other => {}
            }
        }
        Ok(restored)
    }
}

/// Where the last compaction record starts, searching back from the end of
/// the records after the header at `body`.
fn last_compaction(file: &File, body: u64, len: u64) -> io::Result<Option<u64>> {
    let marker = COMPACTION.len() + 1;
    let mut end = len;
    while end > body {
        // Starting at the header's newline finds a record right after it.
        let start = end.saturating_sub(SEARCH_CHUNK).max(body.saturating_sub(1));
        // Chunks overlap by a marker, so one across a boundary is found.
        let stop = (end + marker as u64).min(len);
        let mut chunk = vec![0; usize::try_from(stop - start).map_err(io::Error::other)?];
        file.read_exact_at(&mut chunk, start)?;
        let found = chunk
            .windows(marker)
            .rposition(|window| window[0] == b'\n' && &window[1..] == COMPACTION);
        if let Some(at) = found {
            return Ok(Some(start + at as u64 + 1));
        }
        end = start;
    }
    Ok(None)
}

/// Where session `id` keeps its files, when the id is valid.
pub(crate) fn session_dir(home: &Path, id: &str) -> io::Result<PathBuf> {
    check_id(id)?;
    Ok(home.join("sessions").join(id))
}

/// Checks that `id` is a session id as agt makes them, of digits, lowercase
/// hex digits and `-`, so it names nothing outside the directory it is in.
pub(crate) fn check_id(id: &str) -> io::Result<()> {
    let valid = !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte) || byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::InvalidInput, format!("invalid session id {id:?}")))
    }
}

/// A session as lists show it.
pub(crate) struct Listing {
    pub(crate) id: String,
    pub(crate) cwd: PathBuf,
    modified: SystemTime,
    pub(crate) title: String,
}

impl Listing {
    /// How long before `now` the session was last used, in a few characters,
    /// such as `3h ago`.
    pub(crate) fn ago(&self, now: SystemTime) -> String {
        match now.duration_since(self.modified).map_or(0, |age| age.as_secs()) {
            0..60 => "now".to_owned(),
            seconds @ 60..3_600 => format!("{}m ago", seconds / 60),
            seconds @ 3_600..86_400 => format!("{}h ago", seconds / 3_600),
            seconds => format!("{}d ago", seconds / 86_400),
        }
    }
}

/// Sessions newest first.
pub(crate) fn list(home: &Path) -> io::Result<Vec<Listing>> {
    let entries = match fs::read_dir(home.join("sessions")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut sessions: Vec<Listing> =
        entries.filter_map(|entry| listing(&entry.ok()?.path())).collect();
    sessions.sort_by_key(|session| std::cmp::Reverse(session.modified));
    Ok(sessions)
}

/// The most recently used session started in `cwd`.
pub(crate) fn latest(home: &Path, cwd: &Path) -> io::Result<Option<String>> {
    Ok(list(home)?.into_iter().find(|session| session.cwd == cwd).map(|session| session.id))
}

/// Deletes session `id` and its files. A session that does not exist counts
/// as deleted.
pub(crate) fn delete(home: &Path, id: &str) -> io::Result<()> {
    match fs::remove_dir_all(session_dir(home, id)?) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

/// A short title for a session: the first line of a user message, passing
/// over the headers of images attached to it.
pub(crate) fn title(text: &str) -> String {
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty() && !line.starts_with(crate::image::HEADER))
        .unwrap_or_default();
    let mut chars = line.trim().chars();
    let mut title: String = chars.by_ref().take(TITLE_CHARS).collect();
    if chars.next().is_some() {
        title.pop();
        title.push('…');
    }
    title
}

fn listing(dir: &Path) -> Option<Listing> {
    let saved = Saved::at(dir.to_path_buf()).ok()?;
    let modified = saved.file.metadata().ok()?.modified().ok()?;
    let mut title = None;
    let mut lines = 0;
    // Only the start of the log is read for the first message.
    let mut reader = BufReader::new(&saved.file);
    reader.seek(SeekFrom::Start(saved.body)).ok()?;
    let mut line = Vec::new();
    while title.is_none() && lines < 64 {
        line.clear();
        if reader.read_until(b'\n', &mut line).ok()? == 0 {
            break;
        }
        lines += 1;
        if let Ok(Record::Item { item, origin: Origin::User, .. }) = serde_json::from_slice(&line)
            && item.kind() == Kind::User
            && !agent::is_notices(&item)
        {
            title = Some(self::title(&item.text()));
        }
    }
    Some(Listing {
        id: dir.file_name()?.to_str()?.to_owned(),
        cwd: saved.cwd,
        modified,
        title: title.unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::item::input_text;

    fn message(text: &str) -> Item {
        Item::user(vec![input_text(text)])
    }

    fn item<'a>(item: &'a Item) -> Record<'a> {
        Record::Item { at: 1, origin: Origin::User, item: Cow::Borrowed(item) }
    }

    fn texts(restored: &Restored) -> Vec<String> {
        restored.history.iter().map(Item::text).collect()
    }

    fn compaction<'a>(history: &'a [Item], users: &'a [String]) -> Record<'a> {
        Record::Compaction(Snapshot {
            history: Cow::Borrowed(history),
            summary: "summary".into(),
            users: Cow::Borrowed(users),
            provider: "p".into(),
            model: "m2".into(),
            reasoning_from: 1,
            cost: 0.75,
        })
    }

    #[test]
    fn resume_rebuilds_context_from_the_last_compaction() {
        let home = tempfile::tempdir().expect("temp dir");
        let mut session = Session::create(home.path(), Path::new("/work/old")).expect("session");
        let model =
            |model: &'static str| Record::Model { provider: "p".into(), model: model.into() };
        session.write(&model("m1")).expect("model");
        for text in ["one", "two"] {
            session.write(&item(&message(text))).expect("append");
        }
        session.write(&model("m2")).expect("model");
        session.write(&item(&message("three"))).expect("append");
        session.write(&Record::Cost { usd: 0.25 }).expect("cost");
        session.write(&Record::Cost { usd: 0.5 }).expect("cost");
        let compacted = [message("<checkpoint>summary</checkpoint>"), message("three")];
        let users = ["one".to_owned(), "two".to_owned()];
        session.write(&compaction(&compacted, &users)).expect("compaction");
        session.write(&item(&message("four"))).expect("append");
        session.write(&Record::Cost { usd: 0.125 }).expect("cost");
        let id = session.id.clone();
        drop(session);

        let mut replayed = Vec::new();
        let cwd = Path::new("/work/new");
        let mut replay = |record: &Record<'_>| {
            if let Record::Item { item, .. } = record {
                replayed.push(item.text());
            }
        };
        let (session, full) =
            Session::resume(home.path(), &id, cwd, Some(&mut replay)).expect("resume");
        assert_eq!(replayed, ["one", "two", "three", "four"]);
        assert_eq!(session.cwd, cwd);
        let (_, fast) = Session::resume(home.path(), &id, cwd, None).expect("resume");
        for restored in [full, fast] {
            assert_eq!(texts(&restored), ["<checkpoint>summary</checkpoint>", "three", "four"]);
            assert_eq!(restored.summary, "summary");
            assert_eq!(restored.users, users);
            let model = ModelUse { provider: "p".into(), model: "m2".into(), since: 1 };
            assert_eq!(restored.model, Some(model));
            assert!((restored.cost - 0.875).abs() < 1e-9);
        }
    }

    #[test]
    fn processes_that_never_exited_are_lost() {
        let home = tempfile::tempdir().expect("temp dir");
        let cwd = Path::new("/p");
        let mut session = Session::create(home.path(), cwd).expect("session");
        for (id, command) in [(1, "make"), (2, "npm run dev")] {
            session.write(&Record::Proc { at: 1, id, command: command.into() }).expect("proc");
        }
        session.write(&Record::Exit { at: 2, id: 1, exit: Exit::Code(0) }).expect("exit");
        let (_, restored) = Session::resume(home.path(), &session.id, cwd, None).expect("resume");
        assert_eq!(restored.lost, [(2, "npm run dev".to_owned())]);
    }

    #[test]
    fn a_damaged_last_compaction_is_read_around() {
        let home = tempfile::tempdir().expect("temp dir");
        let cwd = Path::new("/p");
        let mut session = Session::create(home.path(), cwd).expect("session");
        let first = [message("first compaction")];
        session.write(&compaction(&first, &[])).expect("record");
        // Output larger than one search chunk sits between the records.
        let large = Item::output("c1", "x".repeat(3 * 1024 * 1024).into());
        session.write(&item(&large)).expect("append");
        session.write(&item(&message("after"))).expect("append");
        let second = [message("second compaction")];
        session.write(&compaction(&second, &[])).expect("record");
        session.write(&item(&message("latest"))).expect("append");
        let (_, restored) = Session::resume(home.path(), &session.id, cwd, None).expect("resume");
        assert_eq!(texts(&restored), ["second compaction", "latest"]);

        let mut log = File::options().append(true).open(session.log_path()).expect("log");
        log.write_all(br#"{"type":"compaction","history":[{"type":"mess"#).expect("torn record");
        drop(log);
        let (_, restored) = Session::resume(home.path(), &session.id, cwd, None).expect("resume");
        assert_eq!(texts(&restored), ["second compaction", "latest"]);
        let file = File::open(session.log_path()).expect("log");
        assert_eq!(last_compaction(&file, 0, COMPACTION.len() as u64).expect("search"), None);
    }

    #[test]
    fn a_torn_final_line_is_skipped_and_repaired() {
        let home = tempfile::tempdir().expect("temp dir");
        let mut session = Session::create(home.path(), Path::new("/p")).expect("session");
        session.write(&item(&message("kept"))).expect("append");
        let id = session.id.clone();
        let path = session.log_path();
        drop(session);
        let mut file = File::options().append(true).open(&path).expect("log");
        file.write_all(br#"{"type":"item","item":{"ty"#).expect("torn write");

        let cwd = Path::new("/p");
        let (mut session, restored) = Session::resume(home.path(), &id, cwd, None).expect("resume");
        assert_eq!(restored.history.len(), 1);
        session.write(&item(&message("after"))).expect("append");
        let (_, restored) = Session::resume(home.path(), &id, cwd, None).expect("resume again");
        assert_eq!(texts(&restored), ["kept", "after"]);
    }

    #[test]
    fn resume_reapplies_masks() {
        let home = tempfile::tempdir().expect("temp dir");
        let cwd = Path::new("/work");
        let mut session = Session::create(home.path(), cwd).expect("session");
        let original =
            Item::output("a", format!("[id 1 · exit 0 · 1.0s]\n{}", "x".repeat(4096)).into());
        session.write(&item(&original)).expect("output");
        session.write(&Record::Mask { keep: 0 }).expect("mask");
        session.write(&item(&message("continue"))).expect("append");
        let (_, restored) = Session::resume(home.path(), &session.id, cwd, None).expect("resume");
        assert!(restored.history[0].text().contains("output elided"));
        assert_eq!(restored.history[1].text(), "continue");
    }

    #[test]
    fn complete_records_are_read_as_they_are_written() {
        let home = tempfile::tempdir().expect("temp dir");
        let mut session = Session::create(home.path(), Path::new("/p")).expect("session");
        session.write(&item(&message("one"))).expect("append");
        let saved = Saved::open(home.path(), &session.id).expect("open");
        let mut seen = Vec::new();
        let mut each = |record: Record<'static>| seen.push(record);
        let offset = saved.read(0, &mut each).expect("read");
        let mut log = File::options().append(true).open(session.log_path()).expect("log");
        log.write_all(br#"{"type":"turn","at":2,"#).expect("half a record");
        assert_eq!(saved.read(offset, &mut each).expect("read"), offset, "a partial line waits");
        log.write_all(b"\"stop\":\"end_turn\"}\n").expect("the rest");
        saved.read(offset, &mut each).expect("read");
        assert!(
            matches!(seen[..], [Record::Item { .. }, Record::Turn { stop: Stop::EndTurn, .. }]),
            "{seen:?}"
        );
    }

    #[test]
    fn listing_finds_the_latest_session_per_directory() {
        let home = tempfile::tempdir().expect("temp dir");
        let mut first = Session::create(home.path(), Path::new("/a")).expect("session");
        first.write(&item(&message("\nfix the parser\nplease"))).expect("append");
        let other = Session::create(home.path(), Path::new("/b")).expect("session");
        let sessions = list(home.path()).expect("list");
        assert_eq!(sessions.len(), 2);
        let a = sessions.iter().find(|s| s.cwd == Path::new("/a")).expect("a");
        assert_eq!(a.title, "fix the parser");
        let long = title(&"x".repeat(TITLE_CHARS + 1));
        assert_eq!(long, format!("{}…", "x".repeat(TITLE_CHARS - 1)), "a cut title says so");
        assert_eq!(latest(home.path(), Path::new("/b")).expect("latest"), Some(other.id));
        assert_eq!(latest(home.path(), Path::new("/c")).expect("latest"), None);
    }

    #[test]
    fn deleted_sessions_are_gone_and_stay_deleted() {
        let home = tempfile::tempdir().expect("temp dir");
        let session = Session::create(home.path(), Path::new("/p")).expect("session");
        delete(home.path(), &session.id).expect("delete");
        assert!(list(home.path()).expect("list").is_empty());
        delete(home.path(), &session.id).expect("deleting again succeeds");
    }

    #[test]
    fn session_ids_cannot_escape_the_sessions_directory() {
        let home = tempfile::tempdir().expect("temp dir");
        for id in ["../x", "", "a/b", "ABC"] {
            let resumed = Session::resume(home.path(), id, Path::new("/"), None);
            let error = resumed.err().expect("resuming is rejected");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "resume {id:?}: {error}");
            let error = delete(home.path(), id).expect_err("deleting is rejected");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "delete {id:?}: {error}");
        }
    }

    #[test]
    fn invalid_log_records_fail_the_resume() {
        for record in [
            json!({"type":"item","at":1,"item":null}),
            json!({"type":"compaction","history":"all"}),
            json!({"type":"mask","keep":-1}),
            json!({"type":"session","version":1,"cwd":"/p","created":1}),
        ] {
            let home = tempfile::tempdir().expect("temp dir");
            let session = Session::create(home.path(), Path::new("/p")).expect("session");
            let mut log = File::options().append(true).open(session.log_path()).expect("log");
            writeln!(log, "{record}").expect("invalid record");
            let error = Session::resume(home.path(), &session.id, Path::new("/p"), None)
                .err()
                .expect("reject invalid log");
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{record}: {error}");
        }
        let home = tempfile::tempdir().expect("temp dir");
        let dir = home.path().join("sessions/1-a");
        fs::create_dir_all(&dir).expect("dir");
        fs::write(
            dir.join(LOG),
            "{\"type\":\"session\",\"version\":2,\"cwd\":\"/p\",\"created\":1}\n",
        )
        .expect("log");
        let error = Session::resume(home.path(), "1-a", Path::new("/p"), None)
            .err()
            .expect("reject the header");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn timestamps_are_utc_dates_and_minutes() {
        assert_eq!(timestamp(0), "1970-01-01 00:00 UTC");
        assert_eq!(timestamp(1_789_464_757_000), "2026-09-15 09:32 UTC");
        assert_eq!(timestamp(951_782_400_000), "2000-02-29 00:00 UTC");
    }
}
