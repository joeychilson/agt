//! `agt login` and `agt logout`: signing in to providers from the command
//! line, or through the terminal UI's setup menus.

use std::io::{self, BufRead, IsTerminal, Read};
use std::process::ExitCode;
use std::sync::{Arc, mpsc};
use std::thread;

use lexopt::{Arg, Parser, ValueExt};

use super::{Error, failed, load, show_help, working_directory};
use crate::auth::{Credential, Done, Login};
use crate::config::{self, Overrides};
use crate::provider::{Access, Provider};
use crate::tui;

pub(super) const HELP: &str = "\
Usage:
  agt login                      choose a provider, sign in and choose a model, in menus
  agt login <provider>           sign in to a provider in the browser
  agt login <provider> --key -   save an API key for a provider, read from stdin
  agt logout <provider>          forget a provider's saved sign-in or key

The providers are openai (an API key), codex (a ChatGPT Plus or Pro
subscription), grok (a SuperGrok or X Premium+ subscription), openrouter (a
browser sign-in or an API key) and vercel (an API key). Browser sign-in opens
the browser; when it runs on another machine, paste the address it ends on.
Sign-ins and keys are saved in ~/.agt/auth.json, which only you can read.
Choose a model with agt models use.";

/// Runs `agt login`.
pub(super) fn login(mut args: Parser) -> Result<ExitCode, Error> {
    let (mut provider, mut key) = (None, false);
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('h') | Arg::Long("help") => return show_help(HELP),
            Arg::Long("key") => {
                if args.value()?.string()? != "-" {
                    return Err(Error::Usage(
                        "--key reads the key from stdin, so it stays out of the process list: pass --key -".into(),
                    ));
                }
                key = true;
            }
            Arg::Value(value) if provider.is_none() => {
                let id = value.string()?;
                provider = Some(config::provider("the provider", &id).map_err(Error::Usage)?);
            }
            arg => return Err(arg.unexpected().into()),
        }
    }
    let Some(provider) = provider else {
        if key {
            return Err(Error::Usage(
                "--key needs a provider, such as agt login openrouter --key -".into(),
            ));
        }
        let config = load(Overrides::default())?;
        return tui::run(config, &working_directory()?, None, None, true).map_err(failed);
    };
    let spec = provider.spec();
    let credential = match (&spec.access, key) {
        (Access::Subscription(_), true) => {
            return Err(Error::Usage(format!(
                "{} signs in with the browser: agt login {}",
                spec.name, spec.id
            )));
        }
        (Access::Key { sign_in: None, .. }, false) => {
            return Err(Error::Usage(format!(
                "{} takes an API key: pipe it to agt login {} --key -",
                spec.name, spec.id
            )));
        }
        (Access::Key { .. }, true) => read_key()?,
        _ => browser(provider)?,
    };
    let home = config::home()?;
    config::save_credential(&home, provider, &credential)
        .map_err(|error| failed(format!("cannot save the sign-in: {error}")))?;
    println!("signed in to {}; agt models lists its models", spec.name);
    Ok(ExitCode::SUCCESS)
}

/// Runs `agt logout`.
pub(super) fn logout(mut args: Parser) -> Result<ExitCode, Error> {
    let mut provider = None;
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('h') | Arg::Long("help") => return show_help(HELP),
            Arg::Value(value) if provider.is_none() => {
                let id = value.string()?;
                provider = Some(config::provider("the provider", &id).map_err(Error::Usage)?);
            }
            arg => return Err(arg.unexpected().into()),
        }
    }
    let Some(provider) = provider else {
        return Err(Error::Usage("logout takes a provider, such as agt logout grok".into()));
    };
    let home = config::home()?;
    let name = provider.spec().name;
    match config::remove_credential(&home, provider) {
        Ok(true) => println!("signed out of {name}"),
        Ok(false) => return Err(failed(format!("no sign-in or key for {name} is saved"))),
        Err(error) => return Err(failed(format!("cannot save the sign-ins: {error}"))),
    }
    Ok(ExitCode::SUCCESS)
}

/// An API key from stdin.
fn read_key() -> Result<Credential, Error> {
    if io::stdin().is_terminal() {
        return Err(Error::Usage("--key - reads the key from stdin; pipe it in".into()));
    }
    let mut key = String::new();
    io::stdin()
        .read_to_string(&mut key)
        .map_err(|error| failed(format!("cannot read the key: {error}")))?;
    let key = key.trim();
    if key.is_empty() {
        return Err(failed("the key on stdin is empty"));
    }
    Ok(Credential::Key(key.to_owned()))
}

/// Signs in to `provider` in the browser, or with the address it ends on
/// pasted on stdin.
fn browser(provider: Provider) -> Result<Credential, Error> {
    let (sender, results) = mpsc::channel();
    let done: Done = Arc::new(move |result| {
        let _ = sender.send(result);
    });
    let login = Arc::new(Login::start(provider, done).map_err(failed)?);
    eprintln!(
        "Sign in to {} in the browser:\n  {}\nIf the browser runs on another machine, paste the address it ends on here.",
        provider.spec().name,
        login.url
    );
    tui::open(&login.url);
    let pasted = Arc::clone(&login);
    // Left to end with the process once the sign-in is done.
    let _ = thread::Builder::new().name("agt-login-paste".into()).spawn(move || {
        for line in io::stdin().lock().lines().map_while(Result::ok) {
            if !line.trim().is_empty() {
                pasted.paste(&line);
            }
        }
    });
    results.recv().map_err(|_| failed("the sign-in ended without an answer"))?.map_err(failed)
}
