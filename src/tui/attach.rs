//! Images the user attaches to a message: image files dropped on the
//! terminal or pasted as paths, and the image on the system clipboard.
//!
//! They are prepared for the model as `agt view` prepares images, which
//! takes a good part of a second for a large one, so preparing runs on a
//! thread.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

use super::editor::{Part, Source};
use crate::{image, item};

/// Extensions of the image files a paste attaches.
const IMAGE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// The image files `pasted` names, when it names nothing else. A terminal
/// pastes a dropped file as its path escaped as a shell would, and several
/// dropped files as their paths separated by spaces.
pub(crate) fn image_paths(pasted: &str, cwd: &Path, home: Option<&Path>) -> Option<Vec<PathBuf>> {
    let words = shell_words(pasted.trim())?;
    if words.is_empty() {
        return None;
    }
    words
        .into_iter()
        .map(|word| {
            let path = match (word.strip_prefix("~/"), home) {
                (Some(rest), Some(home)) => home.join(rest),
                _ => cwd.join(&word),
            };
            let image = path.extension().and_then(|extension| extension.to_str()).is_some_and(
                |extension| IMAGE_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()),
            );
            (image && path.is_file()).then_some(path)
        })
        .collect()
}

/// Words split at ASCII whitespace outside quotes, with quotes and backslash
/// escapes removed, or `None` when a quote is left open.
fn shell_words(text: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quote = None;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(open), c) if c == open => quote = None,
            (None | Some('"'), '\\') => {
                word.push(chars.next()?);
                started = true;
            }
            (None, '\'' | '"') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_ascii_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            (_, c) => {
                word.push(c);
                started = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if started {
        words.push(word);
    }
    Some(words)
}

/// The image on the system clipboard as PNG, or `None` when it holds none.
/// It runs the platform's clipboard tool, so call it off the event loop.
pub(crate) fn clipboard_image() -> Option<Vec<u8>> {
    let output = |program: &str, args: &[&str]| {
        let output = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        output.status.success().then_some(output.stdout)
    };
    if cfg!(target_os = "macos") {
        let text = output("osascript", &["-e", "the clipboard as «class PNGf»"])?;
        let text = String::from_utf8(text).ok()?;
        let hex = text.trim().strip_prefix("«data PNGf")?.strip_suffix('»')?;
        return decode_hex(hex).filter(|bytes| bytes.starts_with(PNG_SIGNATURE));
    }
    [
        ("wl-paste", &["--no-newline", "--type", "image/png"][..]),
        ("xclip", &["-selection", "clipboard", "-target", "image/png", "-out"][..]),
    ]
    .into_iter()
    .find_map(|(program, args)| output(program, args))
    .filter(|bytes| bytes.starts_with(PNG_SIGNATURE))
}

fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len()).step_by(2).map(|at| u8::from_str_radix(hex.get(at..at + 2)?, 16).ok()).collect()
}

/// A message's content: its text, and its images prepared and saved in the
/// session directory `dir`, in the order they were written.
pub(crate) fn prepare(parts: Vec<Part>, dir: &Path) -> Result<Vec<Value>, String> {
    let mut content = Vec::with_capacity(parts.len());
    for part in parts {
        match part {
            Part::Text(text) => content.push(item::input_text(text)),
            Part::Image(Source::File(path)) => {
                let bytes = fs::read(&path)
                    .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
                let image = image::attach(&bytes, dir)
                    .map_err(|error| format!("cannot use {}: {error}", path.display()))?;
                content.extend(image);
            }
            Part::Image(Source::Clipboard(bytes)) => {
                let image = image::attach(&bytes, dir)
                    .map_err(|error| format!("cannot use the clipboard image: {error}"))?;
                content.extend(image);
            }
        }
    }
    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropped_and_pasted_paths_attach_only_images_that_exist() {
        let dir = tempfile::tempdir().expect("temp dir");
        let shot = dir.path().join("Screenshot 2026-09-14 at 1.02.03\u{202f}PM.png");
        ::image::RgbImage::new(2, 2)
            .save_with_format(&shot, ::image::ImageFormat::Png)
            .expect("png");
        fs::write(dir.path().join("notes.txt"), "x").expect("text");
        fs::write(dir.path().join("b.JPG"), "not really").expect("jpg");
        let escaped = shot.display().to_string().replace(' ', "\\ ");
        assert_eq!(
            image_paths(&format!("{escaped} \n"), dir.path(), None),
            Some(vec![shot.clone()])
        );
        let quoted = format!("'{}' b.JPG", shot.display());
        assert_eq!(
            image_paths(&quoted, dir.path(), None),
            Some(vec![shot.clone(), dir.path().join("b.JPG")])
        );
        assert_eq!(
            image_paths("~/b.JPG", Path::new("/"), Some(dir.path())),
            Some(vec![dir.path().join("b.JPG")])
        );
        assert_eq!(image_paths("notes.txt", dir.path(), None), None);
        assert_eq!(image_paths("look at b.JPG", dir.path(), None), None);
        assert_eq!(image_paths("missing.png", dir.path(), None), None);
        assert_eq!(image_paths("'open", dir.path(), None), None);
        assert_eq!(image_paths("  ", dir.path(), None), None);
    }

    #[test]
    fn shell_words_follow_quotes_and_escapes() {
        assert_eq!(
            shell_words(r#"a\ b "c d" 'e\f' "g\"h""#),
            Some(vec!["a b".into(), "c d".into(), "e\\f".into(), "g\"h".into()])
        );
        assert_eq!(shell_words("trailing\\"), None);
        assert_eq!(decode_hex("89504e47"), Some(vec![0x89, 0x50, 0x4e, 0x47]));
        assert_eq!(decode_hex("895"), None);
        assert_eq!(decode_hex("zz"), None);
    }

    #[test]
    fn prepared_messages_keep_text_and_images_in_order() {
        let dir = tempfile::tempdir().expect("temp dir");
        let shot = dir.path().join("shot.png");
        ::image::RgbImage::new(4, 2).save(&shot).expect("png");
        let parts = vec![
            Part::Text("before ".into()),
            Part::Image(Source::File(shot)),
            Part::Text(" after".into()),
        ];
        let content = prepare(parts, dir.path()).expect("prepared");
        let kinds: Vec<&str> = content.iter().filter_map(|part| part["type"].as_str()).collect();
        assert_eq!(kinds, ["input_text", "input_text", "input_image", "input_text"]);
        assert!(content[1]["text"].as_str().is_some_and(|text| text.starts_with(image::HEADER)));
        let missing = vec![Part::Image(Source::File(dir.path().join("gone.png")))];
        assert!(prepare(missing, dir.path()).is_err_and(|error| error.starts_with("cannot read")));
    }
}
