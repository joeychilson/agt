//! `agt models`: the models that can be used, and choosing the one to use.

use std::process::ExitCode;
use std::thread;

use lexopt::{Arg, Parser, ValueExt};

use super::{Error, check_effort, failed, show_help};
use crate::auth::Auth;
use crate::config::{self, Config, Overrides};
use crate::models::{self, Effort, Model};
use crate::provider::Provider;

/// The most models of a provider listed without a text to choose them.
const LIMIT: usize = 25;
/// The table's columns; the numbers line up on their right.
const HEADINGS: [&str; 7] = ["provider", "model", "context", "in $/M", "out $/M", "efforts", ""];
const NUMBERS: std::ops::Range<usize> = 2..5;

pub(super) const HELP: &str = "\
Usage:
  agt models [<text>]                                   list the models you can use
  agt models use <model> [--provider <id>] [-e <effort>]  use a model from now on

Lists the models of the providers you are signed in to, and of the provider in
use: each model's context window, its prices in dollars per million input and
output tokens, and the reasoning efforts it takes. The first line says which
model is in use. With <text>, lists only the models whose id contains it.

use saves the model, on the provider in use unless --provider names another,
and with -e its reasoning effort, for agt, agt -p and agt acp. A model for one
run is chosen with agt --provider <id> -m <model> instead.";

pub(super) fn run(mut args: Parser) -> Result<ExitCode, Error> {
    if args.raw_args()?.next_if(|arg| arg == "use").is_some() {
        return choose(args);
    }
    let mut text = None;
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('h') | Arg::Long("help") => return show_help(HELP),
            Arg::Value(value) if text.is_none() => text = Some(value.string()?.to_lowercase()),
            arg => return Err(arg.unexpected().into()),
        }
    }
    let config = Config::load(config::home()?, Overrides::default())?;
    println!("{}", in_use(&config));
    let providers = config::usable_providers(&config.home, config.endpoint.provider);
    // Providers that list their models are asked at once.
    let listings: Vec<_> = thread::scope(|scope| {
        let listing: Vec<_> = providers
            .iter()
            .map(|&provider| {
                (provider, scope.spawn(move || models::list(provider, &config::base_url(provider))))
            })
            .collect();
        listing
            .into_iter()
            .map(|(provider, thread)| (provider, thread.join().expect("listing does not panic")))
            .collect()
    });
    let current = config.model.as_ref().map(|model| (config.endpoint.provider, model.id.as_str()));
    let mut rows = vec![HEADINGS.map(str::to_owned)];
    let mut notes = Vec::new();
    let mut listed = false;
    for (provider, listing) in listings {
        let id = provider.spec().id;
        let models = match listing {
            Ok(models) => models,
            Err(error) => {
                eprintln!("agt models: cannot list {}'s models: {error}", provider.spec().name);
                continue;
            }
        };
        listed = true;
        let chosen: Vec<&Model> = models
            .iter()
            .filter(|model| text.as_ref().is_none_or(|text| model.id.to_lowercase().contains(text)))
            .collect();
        let shown = if text.is_some() { chosen.len() } else { chosen.len().min(LIMIT) };
        for model in &chosen[..shown] {
            let current = current == Some((provider, model.id.as_str()));
            rows.push(row(id, model, current));
        }
        if shown < chosen.len() {
            let more = chosen.len() - shown;
            notes.push(format!(
                "{id}: {more} more; agt models <text> lists the ids that contain it"
            ));
        }
    }
    if rows.len() == 1 {
        match &text {
            Some(text) if listed => println!("no model's id contains {text:?}"),
            _ if listed => println!("no models are listed"),
            _ => {}
        }
    } else {
        println!("{}", table(&rows));
    }
    for note in notes {
        println!("{note}");
    }
    Ok(if listed { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

/// What `config` uses, in a line.
fn in_use(config: &Config) -> String {
    let name = config.endpoint.provider.spec().name;
    let signed_in =
        if config.endpoint.auth == Auth::None { ", which you are not signed in to" } else { "" };
    match &config.model {
        Some(model) => {
            let effort = config.reasoning.map(|effort| format!(" at {} effort", effort.as_str()));
            format!("using {}{} on {name}{signed_in}", model.id, effort.unwrap_or_default())
        }
        None => format!("no model is chosen; the provider is {name}{signed_in}"),
    }
}

/// Saves the model the command line names, and its effort.
fn choose(mut args: Parser) -> Result<ExitCode, Error> {
    let (mut id, mut provider, mut effort) = (None, None, None);
    while let Some(arg) = args.next()? {
        match arg {
            Arg::Short('h') | Arg::Long("help") => return show_help(HELP),
            Arg::Long("provider") => {
                let value = args.value()?.string()?;
                provider = Some(config::provider("--provider", &value).map_err(Error::Usage)?);
            }
            Arg::Short('e') | Arg::Long("effort") => {
                let value = args.value()?.string()?;
                effort = Some(config::effort("--effort", &value).map_err(Error::Usage)?);
            }
            Arg::Value(value) if id.is_none() => id = Some(value.string()?),
            arg => return Err(arg.unexpected().into()),
        }
    }
    let Some(id) = id else {
        return Err(Error::Usage("use takes the id of a model, as agt models lists it".into()));
    };
    let config = Config::load(config::home()?, Overrides::default())?;
    let provider: Provider = provider.unwrap_or(config.endpoint.provider);
    let listed = models::list(provider, &config::base_url(provider))
        .ok()
        .and_then(|models| models.into_iter().find(|model| model.id == id));
    let known = listed.is_some();
    let model = listed.unwrap_or_else(|| models::find(provider, &id));
    if let Some(effort) = effort {
        check_effort(&model.id, models::efforts(Some(&model)), effort)?;
    }
    let home = &config.home;
    config::save_model(home, provider, &model)
        .and_then(|()| config::save_effort(home, effort.or(keep_effort(&config, &model))))
        .map_err(|error| failed(format!("cannot save settings: {error}")))?;
    let name = provider.spec().name;
    let effort = effort.map(|effort| format!(" at {} effort", effort.as_str()));
    println!("using {id}{} on {name} from now on", effort.unwrap_or_default());
    if !known {
        let window = models::token_count(model.window);
        println!(
            "{name} does not list {id}, so agt assumes a {window} context window and every effort"
        );
    }
    if !config::signed_in(home, provider) {
        println!("sign in first with agt login {}", provider.spec().id);
    }
    Ok(ExitCode::SUCCESS)
}

/// The saved effort, when the new model takes it.
fn keep_effort(config: &Config, model: &Model) -> Option<Effort> {
    config.reasoning.filter(|effort| models::efforts(Some(model)).contains(effort))
}

/// A model's row: its provider, id, context window, prices, efforts, and
/// whether it is the one in use or takes only text.
fn row(provider: &str, model: &Model, current: bool) -> [String; 7] {
    let prices = model.pricing.map(|pricing| pricing.base);
    let price = |rate: fn(&models::Rates) -> f64| {
        prices.as_ref().map(|rates| models::price(rate(rates))).unwrap_or_default()
    };
    let efforts: Vec<&str> = model.efforts.iter().map(|effort| effort.as_str()).collect();
    let mut notes = Vec::new();
    if current {
        notes.push("current");
    }
    if !model.images {
        notes.push("text only");
    }
    [
        provider.to_owned(),
        model.id.clone(),
        models::token_count(model.window),
        price(|rates| rates.input),
        price(|rates| rates.output),
        efforts.join(","),
        notes.join(", "),
    ]
}

/// Rows as columns of text, each as wide as its widest cell.
fn table(rows: &[[String; 7]]) -> String {
    let widths: Vec<usize> = (0..HEADINGS.len())
        .map(|column| rows.iter().map(|row| row[column].chars().count()).max().unwrap_or(0))
        .collect();
    let lines: Vec<String> = rows
        .iter()
        .map(|row| {
            let cells: Vec<String> = row
                .iter()
                .zip(&widths)
                .enumerate()
                .map(|(column, (cell, &width))| {
                    if NUMBERS.contains(&column) {
                        format!("{cell:>width$}")
                    } else {
                        format!("{cell:<width$}")
                    }
                })
                .collect();
            cells.join("  ").trim_end().to_owned()
        })
        .collect();
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_line_up_in_a_table_with_prices_efforts_and_notes() {
        let luna = models::find(Provider::OpenAi, "gpt-5.6-luna");
        let sol = models::find(Provider::Codex, "gpt-5.6-sol");
        let text = Model { images: false, ..models::find(Provider::OpenRouter, "z-ai/glm-5.3") };
        let rows = [
            HEADINGS.map(str::to_owned),
            row("openai", &luna, false),
            row("codex", &sol, true),
            row("openrouter", &text, false),
        ];
        assert_eq!(
            table(&rows),
            "\
provider    model         context  in $/M  out $/M  efforts\n\
openai      gpt-5.6-luna    1.05M   $0.20    $1.20  none,low,medium,high,xhigh,max\n\
codex       gpt-5.6-sol      272k                   none,low,medium,high,xhigh,max  current\n\
openrouter  z-ai/glm-5.3     200k                                                   text only"
        );
    }
}
