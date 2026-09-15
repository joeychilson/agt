//! `agt view`: showing images to the agent of the session running the command.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use lexopt::{Arg, Parser, ValueExt};

use super::{Error, failed, show_help, working_directory};
use crate::bash::SESSION_DIR;
use crate::image::{self, Terminal};

const REGION: &str = "--region takes left,top,right,bottom in pixels of the original, with left < right and top < bottom, such as --region 0,0,400,300";

pub(super) const HELP: &str = "\
Usage: agt view [--region <left,top,right,bottom>] <image>...

Shows images to the agent of the agt session running this command: each is
attached to the command's output after a header that names it. PNG, JPEG, WebP
and GIF files are supported. --region shows that part of each image at full
resolution, in pixels of the original.";

/// Runs `agt view` with the arguments after `view`.
pub(super) fn run(args: Parser) -> Result<ExitCode, Error> {
    let Some(request) = Request::parse(args)? else {
        return show_help(HELP);
    };
    let dir = std::env::var_os(SESSION_DIR).ok_or_else(|| {
        failed("this shows images to the agent of an agt session, so it runs only in the agent's commands")
    })?;
    let cwd = working_directory()?;
    let mut terminal = Terminal::open().map_err(failed)?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut code = ExitCode::SUCCESS;
    for file in &request.files {
        let path = image::resolve(file, &cwd, home.as_deref());
        let shown = image::show(&path, request.region, Path::new(&dir)).and_then(|shown| {
            terminal
                .show(&shown)
                .map_err(|error| format!("cannot show {}: {error}", file.display()))
        });
        if let Err(error) = shown {
            eprintln!("agt view: {error}");
            code = ExitCode::FAILURE;
        }
    }
    Ok(code)
}

/// What `agt view` was asked to show.
#[derive(Debug, PartialEq)]
struct Request {
    files: Vec<PathBuf>,
    /// `[left, top, right, bottom]` in pixels of each upright original.
    region: Option<[u32; 4]>,
}

impl Request {
    /// Reads the arguments after `view`; `None` asks for help.
    fn parse(mut args: Parser) -> Result<Option<Self>, Error> {
        let mut files = Vec::new();
        let mut region = None;
        while let Some(arg) = args.next()? {
            match arg {
                Arg::Short('h') | Arg::Long("help") => return Ok(None),
                Arg::Long("region") => {
                    let value = args.value()?.string()?;
                    region = Some(parse_region(&value).map_err(Error::Usage)?);
                }
                Arg::Value(file) => files.push(PathBuf::from(file)),
                arg => return Err(arg.unexpected().into()),
            }
        }
        if files.is_empty() {
            return Err(Error::Usage("name an image file to show".into()));
        }
        Ok(Some(Self { files, region }))
    }
}

/// A region written as `left,top,right,bottom`, whose numbers may be
/// fractional, as read off a scaled view.
fn parse_region(text: &str) -> Result<[u32; 4], String> {
    let numbers: Option<Vec<u32>> = text
        .split(',')
        .map(|number| {
            let number = number.trim().parse::<f64>().ok()?;
            (number.is_finite() && number >= 0.0).then(|| number.round() as u32)
        })
        .collect();
    match numbers.as_deref() {
        Some(&[left, top, right, bottom]) if left < right && top < bottom => {
            Ok([left, top, right, bottom])
        }
        _ => Err(REGION.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_name_files_and_a_region_and_errors_say_what_to_pass() {
        let parse = |args: &[&str]| Request::parse(Parser::from_args(args));
        let request = |files: &[&str], region| {
            Ok(Some(Request { files: files.iter().map(PathBuf::from).collect(), region }))
        };
        assert_eq!(parse(&["shot.png"]), request(&["shot.png"], None));
        assert_eq!(
            parse(&["--region", "10.4, 20,30,40", "a.png", "b.png"]),
            request(&["a.png", "b.png"], Some([10, 20, 30, 40]))
        );
        assert_eq!(
            parse(&["a.png", "--region=0,0,5,5", "--", "-dash.png"]),
            request(&["a.png", "-dash.png"], Some([0, 0, 5, 5]))
        );
        assert_eq!(parse(&["--help"]), Ok(None));
        for (args, error) in [
            (&[][..], "name an image file to show"),
            (&["--region"][..], "missing argument for option '--region'"),
            (&["--region", "1,2,3", "a.png"][..], REGION),
            (&["--region", "5,0,5,10", "a.png"][..], REGION),
            (&["--region", "-1,0,5,10", "a.png"][..], REGION),
            (&["--region=all", "a.png"][..], REGION),
            (&["--zoom", "a.png"][..], "invalid option '--zoom'"),
        ] {
            assert_eq!(parse(args), Err(Error::Usage(error.to_owned())), "{args:?}");
        }
    }
}
