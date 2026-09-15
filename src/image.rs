//! Images the model sees: files `agt view` shows it from a command, images
//! the user attaches, and images MCP tools return.
//!
//! An image is decoded, turned upright, cropped to the region asked for and
//! scaled to what providers take at full detail, then saved in the session's
//! `images/` directory. History refers to the saved file and each request
//! reads it back in, so the log stays small, memory stays flat, and a
//! screenshot overwritten later cannot change what the model saw.
//!
//! A command shows an image by writing a marker to its terminal: a private
//! escape sequence naming the saved file. The session reading that terminal
//! puts the image at that point of the command's output. Pipes and
//! redirections cannot take the marker, and decoding runs in the command's
//! own process.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{self, Cursor, Write as _};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process;
use std::thread;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Rgb, RgbImage};
use ring::digest::{SHA256, digest};
use serde_json::{Value, json};

use crate::item::{self, Content, Item};

/// Longest side an image is shown at: Claude rejects longer ones in requests
/// that hold more than 20 images.
const MAX_SIDE: u32 = 2000;
/// Most 32 px patches an image is shown at: what OpenAI's models take at high
/// detail without scaling it down again.
const MAX_PATCHES: u32 = 2500;
/// Largest image saved; Claude on Bedrock and Vertex takes 5 MB of base64.
const MAX_BYTES: usize = 3 * 1024 * 1024;
/// Largest file read.
const MAX_FILE: u64 = 64 * 1024 * 1024;
const JPEG_QUALITY: u8 = 85;
/// Most bytes of images kept in the context. Every request carries them all,
/// so older ones are elided before requests grow slow to send.
pub(crate) const CONTEXT_BYTES: u64 = 8 * 1024 * 1024;
/// The session directory's folder of saved images, which references name.
const IMAGES: &str = "images/";
/// How the header that names a shown image begins.
pub(crate) const HEADER: &str = "[image ";
/// Separates the details of a header.
const DETAIL: &str = " · ";
/// Opens a marker: an OSC with a number no terminal uses, followed by the
/// saved file, `;` and the header, and ended by BEL.
const MARKER: &[u8] = b"\x1b]7719;";
const MARKER_END: u8 = 0x07;
/// Longest marker payload read; a longer one is ordinary output.
const MARKER_LIMIT: usize = 8192;

/// Prepares the image at `path` and saves it in session directory `dir`.
pub(crate) fn show(path: &Path, region: Option<[u32; 4]>, dir: &Path) -> Result<Shown, String> {
    let unreadable = |error: io::Error| format!("cannot read {}: {error}", path.display());
    let metadata = fs::metadata(path).map_err(unreadable)?;
    if metadata.is_dir() {
        return Err(format!("{} is a directory, not an image file", path.display()));
    }
    if metadata.len() > MAX_FILE {
        return Err(format!(
            "{} is {} MB, and images over {} MB cannot be viewed",
            path.display(),
            metadata.len() >> 20,
            MAX_FILE >> 20
        ));
    }
    let bytes = fs::read(path).map_err(unreadable)?;
    let image = prepare(&bytes, region)
        .map_err(|error| format!("cannot view {}: {error}", path.display()))?;
    let reference = save(dir, &image.bytes, image.size, image.format)?;
    Ok(Shown { header: header(path, &image), reference })
}

/// The file `file` names for a command in `cwd`, as models write names. A
/// shell leaves `~` in quotes unexpanded, so a name under a literal `~` that
/// does not exist is taken from `home`. macOS names screenshots with a narrow
/// no-break space before AM or PM, which models type as a plain space, so a
/// name that still does not exist is looked for in its directory among names
/// that differ only in their kinds of spaces and quotes.
pub(crate) fn resolve(file: &Path, cwd: &Path, home: Option<&Path>) -> PathBuf {
    let path = cwd.join(file);
    if path.exists() {
        return path;
    }
    let path = match (file.strip_prefix("~"), home) {
        (Ok(rest), Some(home)) => home.join(rest),
        _ => path,
    };
    if path.exists() {
        return path;
    }
    let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|name| name.to_str()))
    else {
        return path;
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return path;
    };
    entries
        .flatten()
        .find(|entry| {
            let entry = entry.file_name();
            entry.to_str().is_some_and(|entry| fold(entry).eq(fold(name)))
        })
        .map_or(path, |entry| entry.path())
}

/// `name` with every kind of space as a plain space and curly quotes straight.
fn fold(name: &str) -> impl Iterator<Item = char> + '_ {
    name.chars().map(|c| match c {
        '\u{2018}' | '\u{2019}' => '\'',
        '\u{201C}' | '\u{201D}' => '"',
        c if c.is_whitespace() => ' ',
        c => c,
    })
}

/// Prepares an image the user attached, returning the parts of their
/// message: its header on a line of its own, then the image.
pub(crate) fn attach(bytes: &[u8], dir: &Path) -> Result<Vec<Value>, String> {
    let shown = save_image(bytes, dir)?;
    let mut content = Content::from(format!("{}\n", shown.header));
    content.push_image(shown.reference);
    Ok(content.into_parts())
}

/// Prepares image `bytes` that come from no file, such as an attachment or
/// an MCP tool's result, and saves them in session directory `dir`. The
/// original is saved too, and the header names it, so the model can view
/// regions of it.
pub(crate) fn save_image(bytes: &[u8], dir: &Path) -> Result<Shown, String> {
    let image = prepare(bytes, None)?;
    let reference = save(dir, &image.bytes, image.size, image.format)?;
    let original = if image.bytes == bytes {
        reference.clone()
    } else {
        save(dir, bytes, image.source, image.original)?
    };
    Ok(Shown { header: header(&dir.join(original), &image), reference })
}

/// The terminal of the command that shows images, which its session reads.
/// A marker written there, rather than to stdout, cannot be taken by a pipe
/// or redirection.
pub(crate) struct Terminal(fs::File);

impl Terminal {
    pub(crate) fn open() -> Result<Self, String> {
        OpenOptions::new()
            .write(true)
            .open("/dev/tty")
            .map(Self)
            .map_err(|error| format!("no terminal to show images on: {error}"))
    }

    /// Shows `shown` to the session.
    pub(crate) fn show(&mut self, shown: &Shown) -> io::Result<()> {
        self.0.write_all(&shown.marker())
    }
}

/// An image ready to show.
struct Image {
    bytes: Vec<u8>,
    format: ImageFormat,
    /// The format of the original file.
    original: ImageFormat,
    /// The upright size of the original.
    source: (u32, u32),
    /// The part of the original shown, when it is not all of it.
    region: Option<[u32; 4]>,
    /// The size shown.
    size: (u32, u32),
}

fn prepare(bytes: &[u8], region: Option<[u32; 4]>) -> Result<Image, String> {
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| error.to_string())?;
    let Some(original) = reader.format().filter(|format| {
        matches!(
            format,
            ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP | ImageFormat::Gif
        )
    }) else {
        return Err(unsupported(bytes));
    };
    let undecodable = |error| format!("the image cannot be decoded: {error}");
    let mut decoder = reader.into_decoder().map_err(undecodable)?;
    let orientation = decoder.orientation().map_err(undecodable)?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(undecodable)?;
    image.apply_orientation(orientation);
    let source = (image.width(), image.height());
    let region = match region {
        Some(region) => clamp(region, source)?,
        None => None,
    };
    if let Some([left, top, right, bottom]) = region {
        image = image.crop_imm(left, top, right - left, bottom - top);
    }
    let shown = (image.width(), image.height());
    let size = fit(shown);
    // An image shown as it is keeps its own file, which is exact and smallest.
    if size == shown
        && region.is_none()
        && orientation == Orientation::NoTransforms
        && original != ImageFormat::Gif
        && bytes.len() <= MAX_BYTES
    {
        let bytes = bytes.to_vec();
        return Ok(Image { bytes, format: original, original, source, region, size });
    }
    if size != shown {
        image = image.resize_exact(size.0, size.1, FilterType::CatmullRom);
    }
    let (bytes, format) = encode(&image, original)?;
    Ok(Image { bytes, format, original, source, region, size })
}

/// `region` within an image of `size`, or `None` when it covers all of it.
/// A region reaching past the edges is cut at them, since coordinates read
/// off a scaled view can overshoot by a pixel or two.
fn clamp(region: [u32; 4], (width, height): (u32, u32)) -> Result<Option<[u32; 4]>, String> {
    let [left, top, right, bottom] = region;
    let clamped = [left, top, right.min(width), bottom.min(height)];
    if clamped[0] >= clamped[2] || clamped[1] >= clamped[3] {
        return Err(format!(
            "region {left},{top},{right},{bottom} lies outside the image, which is {width}x{height}"
        ));
    }
    Ok((clamped != [0, 0, width, height]).then_some(clamped))
}

/// The size an image of `size` is shown at: the largest with the same aspect
/// ratio within `MAX_SIDE` and `MAX_PATCHES`.
fn fit((width, height): (u32, u32)) -> (u32, u32) {
    let fits = |(width, height): (u32, u32)| {
        width <= MAX_SIDE
            && height <= MAX_SIDE
            && width.div_ceil(32) * height.div_ceil(32) <= MAX_PATCHES
    };
    if fits((width, height)) {
        return (width, height);
    }
    let (w, h) = (f64::from(width), f64::from(height));
    let mut scale =
        (f64::from(MAX_SIDE) / w.max(h)).min((f64::from(MAX_PATCHES * 32 * 32) / (w * h)).sqrt());
    loop {
        let size = (((w * scale).round() as u32).max(1), ((h * scale).round() as u32).max(1));
        if fits(size) {
            return size;
        }
        // Rounding up to whole patches can leave the area a little over.
        scale *= 0.99;
    }
}

/// Encodes a changed image: photos as JPEG, and anything else as PNG unless
/// that is too large.
fn encode(image: &DynamicImage, original: ImageFormat) -> Result<(Vec<u8>, ImageFormat), String> {
    let mut bytes = Vec::new();
    if original != ImageFormat::Jpeg {
        image
            .write_to(Cursor::new(&mut bytes), ImageFormat::Png)
            .map_err(|error| error.to_string())?;
        if bytes.len() <= MAX_BYTES {
            return Ok((bytes, ImageFormat::Png));
        }
        bytes.clear();
    }
    JpegEncoder::new_with_quality(&mut bytes, JPEG_QUALITY)
        .encode_image(&opaque(image))
        .map_err(|error| error.to_string())?;
    Ok((bytes, ImageFormat::Jpeg))
}

/// The image on white, since JPEG has no transparency.
fn opaque(image: &DynamicImage) -> RgbImage {
    let rgba = image.to_rgba8();
    RgbImage::from_fn(image.width(), image.height(), |x, y| {
        let [red, green, blue, alpha] = rgba.get_pixel(x, y).0;
        let alpha = u32::from(alpha);
        Rgb([red, green, blue]
            .map(|channel| ((u32::from(channel) * alpha + 255 * (255 - alpha)) / 255) as u8))
    })
}

/// Why `bytes` cannot be viewed, and how to convert them.
fn unsupported(bytes: &[u8]) -> String {
    let start = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]);
    let convert = if bytes.starts_with(b"%PDF") {
        "render its pages first, for example with `pdftoppm -png -r 100 file.pdf page`"
    } else if start.contains("<svg") {
        "rasterize it first, for example with `rsvg-convert -o image.png file.svg`"
    } else {
        "convert it first, for example with `sips -s format png file --out image.png` on macOS or `magick file image.png`"
    };
    format!("it is not a PNG, JPEG, WebP or GIF image; {convert}")
}

/// Saves `bytes` in `dir` under a name made of their hash and the image's
/// size, and returns that name, which history refers to.
fn save(
    dir: &Path,
    bytes: &[u8],
    (width, height): (u32, u32),
    format: ImageFormat,
) -> Result<String, String> {
    let hash: String =
        digest(&SHA256, bytes).as_ref()[..8].iter().map(|byte| format!("{byte:02x}")).collect();
    let extension = format.extensions_str().first().copied().unwrap_or("png");
    let name = format!("{IMAGES}{hash}-{width}x{height}.{extension}");
    let path = dir.join(&name);
    if path.exists() {
        return Ok(name);
    }
    // Written aside and renamed, so a request never reads part of an image,
    // even while another process saves the same one.
    let temp = dir.join(format!("{name}.{}.{:?}.tmp", process::id(), thread::current().id()));
    let saved = fs::create_dir_all(dir.join(IMAGES))
        .and_then(|()| fs::write(&temp, bytes))
        .and_then(|()| fs::rename(&temp, &path));
    if let Err(error) = saved {
        let _ = fs::remove_file(&temp);
        return Err(format!("cannot save the image: {error}"));
    }
    Ok(name)
}

/// The header that names a shown image: where it came from, its size, the
/// region shown and the size and scale it is shown at.
fn header(path: &Path, image: &Image) -> String {
    // A file name may hold control characters, which no marker carries.
    let path: String = path
        .display()
        .to_string()
        .chars()
        .map(|c| if c.is_control() { '\u{FFFD}' } else { c })
        .collect();
    let (width, height) = image.source;
    let mut header = format!("{HEADER}{path}{DETAIL}{width}x{height}");
    let shown = match image.region {
        Some([left, top, right, bottom]) => {
            let _ = write!(header, "{DETAIL}region {left},{top},{right},{bottom}");
            (right - left, bottom - top)
        }
        None => image.source,
    };
    if image.size != shown {
        let scale = f64::from(shown.0) / f64::from(image.size.0);
        let (width, height) = image.size;
        let _ = write!(header, "{DETAIL}shown at {width}x{height}, scale {scale:.2}");
    }
    header.push(']');
    header
}

/// What the header of a shown image says, read back.
#[derive(Debug, PartialEq)]
pub(crate) struct Header<'a> {
    /// The file the image came from.
    pub(crate) path: &'a str,
    /// The size of the original.
    pub(crate) size: (u32, u32),
    /// The region shown, as `left,top,right,bottom`.
    pub(crate) region: Option<&'a str>,
}

impl<'a> Header<'a> {
    /// The header `line` is, or `None` for other text. A path may hold the
    /// separator of details itself, so it ends before the first size.
    pub(crate) fn parse(line: &'a str) -> Option<Self> {
        let inner = line.strip_prefix(HEADER)?.strip_suffix(']')?;
        let mut from = 0;
        loop {
            let at = from + inner[from..].find(DETAIL)?;
            let mut details = inner[at + DETAIL.len()..].split(DETAIL);
            if let Some(size) = details.next().and_then(dimensions) {
                let region = details.find_map(|detail| detail.strip_prefix("region "));
                return Some(Self { path: &inner[..at], size, region });
            }
            from = at + DETAIL.len();
        }
    }
}

/// An image a command showed: its saved file and the header that names it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Shown {
    pub(crate) reference: String,
    pub(crate) header: String,
}

impl Shown {
    /// The marker that shows the image to the session reading the terminal.
    pub(crate) fn marker(&self) -> Vec<u8> {
        [MARKER, self.reference.as_bytes(), b";", self.header.as_bytes(), &[MARKER_END]].concat()
    }

    /// The image a marker's payload shows, unless the payload names no saved
    /// image or holds a control character, which no header has.
    fn parse(payload: &[u8]) -> Option<Self> {
        let (reference, header) = std::str::from_utf8(payload).ok()?.split_once(';')?;
        let valid = is_reference(reference)
            && header.starts_with(HEADER)
            && !header.contains(char::is_control);
        valid.then(|| Self { reference: reference.to_owned(), header: header.to_owned() })
    }
}

/// A run of terminal output, or an image a marker in it showed.
#[derive(Debug, PartialEq)]
pub(crate) enum Piece<'a> {
    Output(Cow<'a, [u8]>),
    Image(Shown),
}

/// Finds the markers in terminal output, which arrives in chunks that may
/// split one.
#[derive(Default)]
pub(crate) struct Markers {
    /// The start of a marker the previous chunk ended in.
    partial: Vec<u8>,
}

impl Markers {
    /// Splits `chunk` into output and the images marked in it, in order,
    /// holding back the start of a marker it ends in.
    pub(crate) fn split<'a>(&mut self, chunk: &'a [u8]) -> Vec<Piece<'a>> {
        if self.partial.is_empty() {
            let (found, rest) = scan(chunk);
            self.partial = chunk[rest..].to_vec();
            return found
                .into_iter()
                .map(|found| match found {
                    Found::Output(range) => Piece::Output(Cow::Borrowed(&chunk[range])),
                    Found::Image(shown) => Piece::Image(shown),
                })
                .collect();
        }
        let mut joined = std::mem::take(&mut self.partial);
        joined.extend_from_slice(chunk);
        let (found, rest) = scan(&joined);
        let pieces = found
            .into_iter()
            .map(|found| match found {
                Found::Output(range) => Piece::Output(Cow::Owned(joined[range].to_vec())),
                Found::Image(shown) => Piece::Image(shown),
            })
            .collect();
        self.partial = joined[rest..].to_vec();
        pieces
    }

    /// The start of a marker the output ended in, which is output after all.
    pub(crate) fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.partial)
    }
}

/// What `scan` finds.
enum Found {
    Output(Range<usize>),
    Image(Shown),
}

/// The output and marked images in `data`, and where a marker `data` ends
/// before completing starts.
fn scan(data: &[u8]) -> (Vec<Found>, usize) {
    let mut found = Vec::new();
    let (mut start, mut from, mut rest) = (0, 0, data.len());
    while let Some(offset) = data[from..].iter().position(|&byte| byte == MARKER[0]) {
        let at = from + offset;
        from = at + 1;
        let tail = &data[at..];
        let Some(payload) = tail.strip_prefix(MARKER) else {
            if MARKER.starts_with(tail) {
                rest = at;
                break;
            }
            continue;
        };
        match payload.iter().take(MARKER_LIMIT + 1).position(|&byte| byte == MARKER_END) {
            Some(end) => {
                let Some(shown) = Shown::parse(&payload[..end]) else { continue };
                if start < at {
                    found.push(Found::Output(start..at));
                }
                found.push(Found::Image(shown));
                start = at + MARKER.len() + end + 1;
                from = start;
            }
            None if payload.len() <= MARKER_LIMIT => {
                rest = at;
                break;
            }
            None => {}
        }
    }
    if start < rest {
        found.push(Found::Output(start..rest));
    }
    (found, rest)
}

/// The part of a message or output that shows saved image `reference`.
pub(crate) fn part(reference: String) -> Value {
    json!({ "type": "input_image", "image_url": reference, "detail": "high" })
}

/// The saved image a part of a message or tool output shows, if it shows one.
pub(crate) fn reference(part: &Value) -> Option<&str> {
    part["image_url"].as_str().filter(|url| part["type"] == "input_image" && is_reference(url))
}

/// The saved images among the parts of a message's content or a tool's output.
pub(crate) fn references(content: &Value) -> impl Iterator<Item = &str> {
    content.as_array().into_iter().flatten().filter_map(reference)
}

/// Whether `url` names a file directly in the session's `images/`, which a
/// damaged log or a forged marker must not be able to point elsewhere.
fn is_reference(url: &str) -> bool {
    url.strip_prefix(IMAGES)
        .is_some_and(|name| !name.is_empty() && !name.contains('/') && !name.starts_with('.'))
}

/// The size of a saved image, from the name `save` gives it.
pub(crate) fn size(reference: &str) -> Option<(u32, u32)> {
    let (_, name) = reference.rsplit_once('-')?;
    let (size, _) = name.split_once('.')?;
    dimensions(size)
}

/// A size written as `<width>x<height>`.
fn dimensions(text: &str) -> Option<(u32, u32)> {
    let (width, height) = text.split_once('x')?;
    Some((width.parse().ok()?, height.parse().ok()?))
}

/// The visual tokens of a saved image: Claude's count of 28 px patches, which
/// is more than OpenAI's models use for it.
pub(crate) fn tokens(reference: &str) -> u64 {
    size(reference).map_or(u64::from(MAX_PATCHES), |(width, height)| {
        u64::from(width.div_ceil(28)) * u64::from(height.div_ceil(28))
    })
}

/// Bytes of the saved images `history` shows.
pub(crate) fn bytes(history: &[Item], dir: &Path) -> u64 {
    history
        .iter()
        .flat_map(Item::images)
        .filter_map(|reference| fs::metadata(dir.join(reference)).ok())
        .map(|metadata| metadata.len())
        .sum()
}

/// `item` as requests carry it: its saved images read from `images`, the
/// session directory, as data URLs, or a note in place of each when the model
/// is sent no images or the file is gone.
pub(crate) fn inline(item: &Item, images: Option<&Path>) -> Item {
    let mut item = item.clone();
    let Some(parts) = item.content_mut().and_then(Value::as_array_mut) else {
        return item;
    };
    for part in parts {
        let Some(reference) = reference(part).map(str::to_owned) else {
            continue;
        };
        let note = match images {
            Some(dir) => match load(dir, &reference) {
                Ok((mime, data)) => {
                    part["image_url"] = format!("data:{mime};base64,{data}").into();
                    continue;
                }
                Err(error) => format!("[image {reference} is unavailable: {error}]"),
            },
            None => "[image omitted: images are not sent to this model]".to_owned(),
        };
        *part = item::input_text(note);
    }
    item
}

/// A saved image's media type and its content in base64.
pub(crate) fn load(dir: &Path, reference: &str) -> io::Result<(&'static str, String)> {
    let bytes = fs::read(dir.join(reference))?;
    let mime = match reference.rsplit_once('.') {
        Some((_, "jpg")) => "image/jpeg",
        Some((_, "webp")) => "image/webp",
        Some((_, "gif")) => "image/gif",
        _ => "image/png",
    };
    Ok((mime, STANDARD.encode(bytes)))
}

#[cfg(test)]
mod tests {
    use image::RgbaImage;

    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image =
            RgbImage::from_fn(width, height, |x, y| Rgb([(x % 256) as u8, (y % 256) as u8, 128]));
        let mut bytes = Vec::new();
        DynamicImage::ImageRgb8(image)
            .write_to(Cursor::new(&mut bytes), ImageFormat::Png)
            .expect("png");
        bytes
    }

    /// A JPEG of `width`×`height` pixels whose EXIF orientation turns it a
    /// quarter clockwise.
    fn rotated_jpeg(width: u32, height: u32) -> Vec<u8> {
        let mut jpeg = Vec::new();
        JpegEncoder::new(&mut jpeg).encode_image(&RgbImage::new(width, height)).expect("jpeg");
        // TIFF header, then one IFD entry: orientation (0x0112), a SHORT, 6.
        let tiff = [
            b'M', b'M', 0, 42, 0, 0, 0, 8, 0, 1, 0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, 6, 0, 0, 0, 0, 0,
            0,
        ];
        let length = u16::try_from(2 + 6 + tiff.len()).expect("length");
        let mut segment = vec![0xff, 0xe1];
        segment.extend(length.to_be_bytes());
        segment.extend(b"Exif\0\0");
        segment.extend(tiff);
        // After the JFIF segment the encoder writes first.
        let jfif_end = 4 + usize::from(u16::from_be_bytes([jpeg[4], jpeg[5]]));
        [&jpeg[..jfif_end], &segment, &jpeg[jfif_end..]].concat()
    }

    fn decoded_size(bytes: &[u8]) -> (u32, u32) {
        let image = image::load_from_memory(bytes).expect("decodes");
        (image.width(), image.height())
    }

    #[test]
    fn images_fit_the_side_and_patch_limits() {
        assert_eq!(fit((1000, 800)), (1000, 800));
        // 2000x1250 would take 63x40 patches, so it shrinks a little more.
        let (width, height) = fit((2880, 1800));
        assert!(width.div_ceil(32) * height.div_ceil(32) <= MAX_PATCHES);
        assert!((1950..2000).contains(&width), "{width}");
        assert_eq!(height, (f64::from(width) / 1.6).round() as u32);
        // The patch limit binds first: 1600x1600 is exactly 50x50 patches.
        assert_eq!(fit((4000, 4000)), (1600, 1600));
        assert_eq!(fit((100, 8000)), (25, 2000));
        assert_eq!(fit((65535, 1)), (2000, 1));
    }

    #[test]
    fn images_are_scaled_cropped_and_turned_upright() {
        // Thin images keep these tests quick in unoptimized builds.
        let large = png(2200, 20);
        let image = prepare(&large, None).expect("prepared");
        assert_eq!((image.source, image.size), ((2200, 20), (2000, 18)));
        assert_eq!(image.format, ImageFormat::Png);
        assert_eq!(decoded_size(&image.bytes), (2000, 18));

        let small = png(100, 50);
        let image = prepare(&small, None).expect("prepared");
        assert_eq!(image.bytes, small, "an image that fits keeps its file");

        let image = prepare(&large, Some([100, 5, 500, 15])).expect("region");
        assert_eq!((image.region, image.size), (Some([100, 5, 500, 15]), (400, 10)));
        let image = prepare(&large, Some([2100, 10, 2300, 30])).expect("region past the edge");
        assert_eq!((image.region, image.size), (Some([2100, 10, 2200, 20]), (100, 10)));
        assert_eq!(prepare(&large, Some([0, 0, 2200, 20])).expect("whole").region, None);
        let outside = prepare(&large, Some([2200, 0, 2300, 10])).err().expect("outside");
        assert_eq!(outside, "region 2200,0,2300,10 lies outside the image, which is 2200x20");

        let image = prepare(&rotated_jpeg(40, 20), None).expect("jpeg");
        assert_eq!((image.source, image.format), ((20, 40), ImageFormat::Jpeg));
        assert_eq!(decoded_size(&image.bytes), (20, 40));
    }

    #[test]
    fn files_that_are_not_images_say_how_to_convert_them() {
        let pdf = prepare(b"%PDF-1.7\n", None).err().expect("pdf");
        assert!(pdf.contains("pdftoppm"), "{pdf}");
        let svg = prepare(b"<?xml version=\"1.0\"?><svg></svg>", None).err().expect("svg");
        assert!(svg.contains("rsvg-convert"), "{svg}");
        let text = prepare(b"plain text", None).err().expect("text");
        assert!(text.starts_with("it is not a PNG, JPEG, WebP or GIF image"), "{text}");
        let truncated = prepare(&png(64, 64)[..60], None).err().expect("truncated");
        assert!(truncated.starts_with("the image cannot be decoded"), "{truncated}");
    }

    #[test]
    fn photos_stay_jpeg_and_transparency_turns_white() {
        let image =
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(4, 2, image::Rgba([255, 0, 0, 128])));
        let (bytes, format) = encode(&image, ImageFormat::Jpeg).expect("jpeg");
        assert_eq!((format, decoded_size(&bytes)), (ImageFormat::Jpeg, (4, 2)));
        let mut pixels = RgbaImage::new(2, 1);
        pixels.put_pixel(0, 0, image::Rgba([0, 0, 0, 0]));
        pixels.put_pixel(1, 0, image::Rgba([255, 0, 0, 128]));
        let flat = opaque(&DynamicImage::ImageRgba8(pixels));
        assert_eq!(flat.get_pixel(0, 0).0, [255, 255, 255]);
        assert_eq!(flat.get_pixel(1, 0).0, [255, 127, 127]);
    }

    #[test]
    fn file_names_are_found_as_models_write_them() {
        let root = tempfile::tempdir().expect("temp dir");
        let (home, cwd) = (root.path(), Path::new("/work"));
        let screenshot = home.join("Screenshot 2026-09-12 at 3.38.32\u{202F}PM.png");
        let curly = home.join("It\u{2019}s a shot.png");
        for file in [&screenshot, &curly] {
            fs::write(file, png(1, 1)).expect("image");
        }
        for (written, path) in [
            (home.join("Screenshot 2026-09-12 at 3.38.32 PM.png"), &screenshot),
            (home.join("It's a shot.png"), &curly),
            (PathBuf::from("~/Screenshot 2026-09-12 at 3.38.32 PM.png"), &screenshot),
        ] {
            assert_eq!(&resolve(&written, cwd, Some(home)), path, "{}", written.display());
        }
        assert_eq!(resolve(Path::new("~/gone.png"), cwd, None), cwd.join("~/gone.png"));
        assert_eq!(resolve(Path::new("gone.png"), cwd, Some(home)), cwd.join("gone.png"));
    }

    #[test]
    fn showing_directories_and_missing_files_says_why() {
        let root = tempfile::tempdir().expect("temp dir");
        let (cwd, dir) = (root.path(), root.path().join("session"));
        let show = |file: &str| show(&cwd.join(file), None, &dir).expect_err("nothing to show");
        let directory = format!("{} is a directory, not an image file", cwd.join(".").display());
        assert_eq!(show("."), directory);
        let missing = show("gone.png");
        let unreadable = format!("cannot read {}: ", cwd.join("gone.png").display());
        assert!(missing.starts_with(&unreadable), "{missing}");
    }

    #[test]
    fn shown_images_are_saved_and_named_by_their_header() {
        let root = tempfile::tempdir().expect("temp dir");
        let (cwd, dir) = (root.path().join("work"), root.path().join("session"));
        fs::create_dir_all(&cwd).expect("cwd");
        let path = cwd.join("shot.png");
        fs::write(&path, png(2200, 20)).expect("image");
        let shown = show(&path, None, &dir).expect("shown");
        let expected =
            format!("[image {} · 2200x20 · shown at 2000x18, scale 1.10]", path.display());
        assert_eq!(shown.header, expected);
        let written = path.display().to_string();
        assert_eq!(
            Header::parse(&shown.header),
            Some(Header { path: &written, size: (2200, 20), region: None })
        );
        let named =
            shown.reference.starts_with("images/") && shown.reference.ends_with("-2000x18.png");
        assert!(named, "{}", shown.reference);
        assert_eq!(decoded_size(&fs::read(dir.join(&shown.reference)).expect("saved")), (2000, 18));
        assert_eq!(tokens(&shown.reference), 72, "Claude's patches of 28 px: 72 by 1");
        let sizeless = tokens("images/sizeless.png");
        assert_eq!(sizeless, u64::from(MAX_PATCHES), "a name without a size counts as the largest");

        let region = show(&path, Some([100, 0, 500, 20]), &dir).expect("region");
        assert_eq!(region.header, format!("[image {written} · 2200x20 · region 100,0,500,20]"));
        assert_eq!(
            Header::parse(&region.header).and_then(|header| header.region),
            Some("100,0,500,20")
        );
        assert_eq!(
            Header::parse("[image /w/a · b.png · 4x2]"),
            Some(Header { path: "/w/a · b.png", size: (4, 2), region: None }),
            "a path may hold the separator"
        );
        for other in ["[id 1 · exit 0 · 0.1s]", "[image /w/a.png]", "[image /w/a.png · 4x2"] {
            assert_eq!(Header::parse(other), None, "{other}");
        }
    }

    #[test]
    fn markers_are_found_however_output_is_split() {
        let first = Shown {
            reference: "images/ab-4x2.png".into(),
            header: "[image /w/a.png · 4x2]".into(),
        };
        let second = Shown {
            reference: "images/cd-1x1.png".into(),
            header: "[image /w/b c.png · 1x1]".into(),
        };
        let forged = Shown { reference: "../log.jsonl".into(), header: "[image x]".into() };
        let parts: [&[u8]; 7] = [
            b"before \x1b[31mred\x1b[0m ",
            &first.marker(),
            b"\x1b]0;title\x07between",
            &forged.marker(),
            &second.marker(),
            &second.marker(),
            b"after\x1b]77",
        ];
        let output = [parts[0], parts[2], parts[3], b"after"].concat();
        let images = [first, second.clone(), second];
        let data = parts.concat();
        for size in [1, 2, 7, data.len()] {
            let mut markers = Markers::default();
            let (mut seen, mut shown) = (Vec::new(), Vec::new());
            for chunk in data.chunks(size) {
                for piece in markers.split(chunk) {
                    match piece {
                        Piece::Output(bytes) => seen.extend_from_slice(&bytes),
                        Piece::Image(image) => shown.push(image),
                    }
                }
            }
            assert_eq!(
                String::from_utf8_lossy(&seen),
                String::from_utf8_lossy(&output),
                "chunks of {size}"
            );
            assert_eq!(shown, images, "chunks of {size}");
            assert_eq!(
                markers.finish(),
                b"\x1b]77",
                "chunks of {size}: an unfinished marker is output"
            );
        }

        let mut markers = Markers::default();
        let long = [MARKER, &[b'x'; MARKER_LIMIT + 1][..]].concat();
        let pieces = markers.split(&long);
        assert_eq!(
            pieces,
            [Piece::Output(Cow::Borrowed(&long[..]))],
            "a marker too long is output"
        );
        assert!(markers.finish().is_empty());
    }

    #[test]
    fn requests_read_saved_images_back_or_say_why_not() {
        let dir = tempfile::tempdir().expect("temp dir");
        let dir = dir.path();
        let item = Item::output("c1", Value::Array(attach(&png(4, 2), dir).expect("attached")));
        let reference = item.content()[1]["image_url"].as_str().expect("reference").to_owned();
        let saved = fs::read(dir.join(&reference)).expect("saved");
        assert_eq!(bytes(std::slice::from_ref(&item), dir), saved.len() as u64);

        let sent = inline(&item, Some(dir));
        let data = format!("data:image/png;base64,{}", STANDARD.encode(&saved));
        assert_eq!(sent.content()[1]["image_url"], data);
        assert_eq!(sent.content()[0], item.content()[0], "the header is sent as it is");
        let hidden = inline(&item, None);
        let note = item::input_text("[image omitted: images are not sent to this model]");
        assert_eq!(hidden.content()[1], note);
        fs::remove_file(dir.join(&reference)).expect("remove");
        let missing = inline(&item, Some(dir));
        let text = missing.content()[1]["text"].as_str().unwrap_or_default();
        assert!(text.starts_with(&format!("[image {reference} is unavailable: ")), "{text}");
    }

    #[test]
    fn image_references_stay_inside_the_images_directory() {
        for url in [
            "images/../log.jsonl",
            "images/",
            "images/.hidden",
            "data:image/png;base64,AA",
            "/etc/passwd",
        ] {
            let part = json!([{ "type": "input_image", "image_url": url }]);
            assert_eq!(references(&part).count(), 0, "{url}");
        }
        let saved = json!([part("images/ab-4x2.png".into())]);
        assert_eq!(references(&saved).collect::<Vec<_>>(), ["images/ab-4x2.png"]);
    }

    #[test]
    fn attachments_keep_their_original_for_regions() {
        let dir = tempfile::tempdir().expect("temp dir");
        let parts = attach(&png(2200, 20), dir.path()).expect("attached");
        let header = parts[0]["text"].as_str().expect("header");
        assert!(
            header.ends_with("-2200x20.png · 2200x20 · shown at 2000x18, scale 1.10]\n"),
            "{header}"
        );
        let original =
            header.strip_prefix(HEADER).and_then(|rest| rest.split(" · ").next()).expect("path");
        assert_eq!(decoded_size(&fs::read(original).expect("original")), (2200, 20));
        let small = attach(&png(10, 10), dir.path()).expect("attached");
        let reference = small[1]["image_url"].as_str().expect("reference");
        assert!(small[0]["text"].as_str().is_some_and(|header| header.contains(reference)));
    }
}
