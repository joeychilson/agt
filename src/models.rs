//! The models each provider offers and what agt needs to know about them: the
//! context window, reasoning efforts, prices, and the input size above which a
//! whole request is billed at higher rates.
//!
//! OpenAI's endpoints describe none of this, so its current models are listed
//! here. So are the two a Grok subscription offers for coding, since xAI lists
//! models only to signed-in clients and mixes in image and video models.
//! OpenRouter and Vercel AI Gateway describe theirs at `GET /models` without a
//! key; only models that call tools and reason are offered.

use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::llm::{self, Usage};
use crate::provider::{Access, Catalog, Provider};

/// The context window assumed for a model no listing describes.
const DEFAULT_WINDOW: u64 = 200_000;
/// The smallest context window agt works with.
pub(crate) const MIN_WINDOW: u64 = 8_000;

const GPT_5_6_EFFORTS: [Effort; 6] =
    [Effort::None, Effort::Low, Effort::Medium, Effort::High, Effort::Xhigh, Effort::Max];
const GPT_6_EFFORTS: [Effort; 5] =
    [Effort::Low, Effort::Medium, Effort::High, Effort::Xhigh, Effort::Max];
/// OpenAI's current models, with the rates for requests above their tier.
pub(crate) const OPENAI: [Known; 4] = [
    Known {
        id: "gpt-5.6-luna",
        efforts: &GPT_5_6_EFFORTS,
        pricing: Some(Pricing {
            base: rates(0.2, 0.02, 0.25, 1.2),
            long: rates(0.4, 0.04, 0.5, 1.8),
        }),
    },
    Known {
        id: "gpt-5.6-sol",
        efforts: &GPT_5_6_EFFORTS,
        pricing: Some(Pricing {
            base: rates(2.0, 0.2, 2.5, 10.0),
            long: rates(4.0, 0.4, 5.0, 15.0),
        }),
    },
    Known {
        id: "gpt-5.6-terra",
        efforts: &GPT_5_6_EFFORTS,
        pricing: Some(Pricing {
            base: rates(2.0, 0.2, 2.5, 12.0),
            long: rates(4.0, 0.4, 5.0, 18.0),
        }),
    },
    Known {
        id: "gpt-6-astra",
        efforts: &GPT_6_EFFORTS,
        pricing: Some(Pricing {
            base: rates(10.0, 1.0, 12.5, 50.0),
            long: rates(20.0, 2.0, 25.0, 75.0),
        }),
    },
];
/// The models a Grok subscription offers for coding.
pub(crate) const GROK: [Known; 2] = [
    Known { id: "grok-4.5", efforts: &[Effort::Low, Effort::Medium, Effort::High], pricing: None },
    Known {
        id: "grok-4.6",
        efforts: &[Effort::Low, Effort::Medium, Effort::High, Effort::Xhigh],
        pricing: None,
    },
];

/// How hard a model reasons before it answers, as requests name it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Effort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    /// Every effort, least first.
    pub(crate) const ALL: [Self; 7] =
        [Self::None, Self::Minimal, Self::Low, Self::Medium, Self::High, Self::Xhigh, Self::Max];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// The effort named `name`.
    pub(crate) fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|effort| effort.as_str() == name)
    }
}

impl Serialize for Effort {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// A model agt knows without a listing.
pub(crate) struct Known {
    id: &'static str,
    /// Accepted reasoning efforts, least first.
    efforts: &'static [Effort],
    pricing: Option<Pricing>,
}

/// A model as agt saves it with the settings; missing fields take defaults.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub(crate) struct Model {
    pub(crate) id: String,
    pub(crate) window: u64,
    /// Accepted reasoning efforts, least first; empty when unknown.
    #[serde(deserialize_with = "known_efforts")]
    pub(crate) efforts: Vec<Effort>,
    /// Input tokens above which a whole request is billed at the long rates.
    pub(crate) tier: Option<u64>,
    pub(crate) pricing: Option<Pricing>,
    /// Whether the model accepts images.
    pub(crate) images: bool,
}

/// Dollars per million tokens.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct Rates {
    pub(crate) input: f64,
    pub(crate) cached: f64,
    pub(crate) cache_write: f64,
    pub(crate) output: f64,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct Pricing {
    pub(crate) base: Rates,
    /// Rates for a request whose input exceeds the model's tier.
    pub(crate) long: Rates,
}

const fn rates(input: f64, cached: f64, cache_write: f64, output: f64) -> Rates {
    Rates { input, cached, cache_write, output }
}

impl Default for Model {
    /// What agt assumes of a model no listing describes, including that it
    /// accepts images until its provider says otherwise.
    fn default() -> Self {
        Self {
            id: String::new(),
            window: DEFAULT_WINDOW,
            efforts: Vec::new(),
            tier: None,
            pricing: None,
            images: true,
        }
    }
}

impl Model {
    /// A model known only by its id.
    fn unknown(id: &str) -> Self {
        Self { id: id.to_owned(), ..Self::default() }
    }

    /// What a response cost in dollars, when the model's prices are known.
    pub(crate) fn cost(&self, usage: &Usage) -> Option<f64> {
        let pricing = self.pricing?;
        let rates = if self.tier.is_some_and(|tier| usage.input > tier) {
            pricing.long
        } else {
            pricing.base
        };
        let fresh = usage.input.saturating_sub(usage.cached.saturating_add(usage.cache_write));
        let dollars = |tokens: u64, rate: f64| tokens as f64 * rate / 1e6;
        Some(
            dollars(fresh, rates.input)
                + dollars(usage.cached, rates.cached)
                + dollars(usage.cache_write, rates.cache_write)
                + dollars(usage.output, rates.output),
        )
    }

    /// Reads a saved model, leaving out efforts agt does not know.
    pub(crate) fn from_json(value: &Value) -> Option<Self> {
        Self::deserialize(value).ok().filter(|model| !model.id.trim().is_empty())
    }
}

/// The efforts a saved model names, without those agt does not know.
fn known_efforts<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<Effort>, D::Error> {
    let names = Vec::<String>::deserialize(deserializer)?;
    Ok(names.iter().filter_map(|name| Effort::parse(name)).collect())
}

/// The efforts to offer for `model`: its own, or every effort when unknown.
pub(crate) fn efforts(model: Option<&Model>) -> &[Effort] {
    match model {
        Some(model) if !model.efforts.is_empty() => &model.efforts,
        _ => &Effort::ALL,
    }
}

/// Approximates the tokens in `bytes` of text, for what no provider counted.
pub(crate) fn tokens(bytes: usize) -> u64 {
    u64::try_from(bytes.div_ceil(4)).unwrap_or(u64::MAX)
}

/// The bytes of text that make about `tokens` tokens, as `tokens` counts them.
pub(crate) fn bytes(tokens: u64) -> usize {
    usize::try_from(tokens.saturating_mul(4)).unwrap_or(usize::MAX)
}

/// A token count in a few characters: `850`, `2.1k`, `200k`, `1.05M`.
pub(crate) fn token_count(count: u64) -> String {
    match count {
        0..1_000 => count.to_string(),
        1_000..10_000 => format!("{:.1}k", count as f64 / 1e3),
        10_000..1_000_000 => format!("{}k", count / 1_000),
        _ => format!("{:.2}M", count as f64 / 1e6),
    }
}

/// Dollars with two decimals, or up to four for a price under a cent.
pub(crate) fn price(dollars: f64) -> String {
    let text = format!("{dollars:.4}");
    let kept = text.trim_end_matches('0').len().max(text.len() - 2);
    format!("${}", &text[..kept])
}

/// The models agt knows on `provider` without asking it, or `None` when the
/// provider lists its own.
pub(crate) fn known(provider: Provider) -> Option<Vec<Model>> {
    let spec = provider.spec();
    let Catalog::Known { models, window, tier } = spec.catalog else {
        return None;
    };
    // A subscription has no per-token price.
    let priced = matches!(spec.access, Access::Key { .. });
    let models = models.iter().map(|model| Model {
        id: model.id.to_owned(),
        window,
        efforts: model.efforts.to_vec(),
        tier: Some(tier),
        pricing: model.pricing.filter(|_| priced),
        images: true,
    });
    Some(models.collect())
}

/// Model `id` on `provider` as far as agt knows it without a listing.
pub(crate) fn find(provider: Provider, id: &str) -> Model {
    let found = known(provider).into_iter().flatten().find(|model| model.id == id);
    found.unwrap_or_else(|| Model::unknown(id))
}

/// The models `provider` offers at `base_url`, sorted by id.
pub(crate) fn list(provider: Provider, base_url: &str) -> Result<Vec<Model>, String> {
    let Catalog::Listed(describe) = provider.spec().catalog else {
        return Ok(known(provider).unwrap_or_default());
    };
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let response = llm::http(Duration::from_secs(10)).get(url).call();
    let listing = llm::json_body(response).map_err(|error| error.message)?;
    parse(&listing, describe)
}

/// The models a listing describes, sorted by id, where `describe` reads an
/// entry of the provider's listing.
fn parse(listing: &Value, describe: fn(&Value) -> Option<Model>) -> Result<Vec<Model>, String> {
    let entries = listing["data"].as_array().ok_or("the model list has no data array")?;
    let mut models: Vec<Model> = entries.iter().filter_map(describe).collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(models)
}

/// An OpenRouter entry. An override with `min_prompt_tokens` gives the rates
/// above that input size.
pub(crate) fn openrouter(entry: &Value) -> Option<Model> {
    let supports = |name: &str| {
        entry["supported_parameters"]
            .as_array()
            .is_some_and(|names| names.iter().any(|value| value.as_str() == Some(name)))
    };
    let id = entry["id"].as_str()?;
    // Batch variants serve asynchronous batch jobs, not an interactive agent.
    if !supports("tools") || !supports("reasoning") || id.ends_with(":batch") {
        return None;
    }
    let keys = ["prompt", "input_cache_read", "input_cache_write", "completion"];
    let price = &entry["pricing"];
    let long = price["overrides"].as_array().and_then(|overrides| {
        overrides
            .iter()
            .filter(|tier| tier["min_prompt_tokens"].is_u64())
            .min_by_key(|tier| tier["min_prompt_tokens"].as_u64())
    });
    let base = read_rates(price, keys, None);
    Some(Model {
        id: id.to_owned(),
        window: entry["context_length"].as_u64().unwrap_or(DEFAULT_WINDOW),
        efforts: named_efforts(&entry["reasoning"]["supported_efforts"]),
        tier: long.and_then(|tier| tier["min_prompt_tokens"].as_u64()),
        pricing: base.map(|base| Pricing {
            base,
            long: long.and_then(|tier| read_rates(tier, keys, Some(base))).unwrap_or(base),
        }),
        images: accepts_images(&entry["architecture"]["input_modalities"]),
    })
}

/// A Vercel AI Gateway entry. Tiered prices list each rate by input size; the
/// tier starting above zero gives the long rates.
pub(crate) fn vercel(entry: &Value) -> Option<Model> {
    let tagged = |tag: &str| {
        entry["tags"]
            .as_array()
            .is_some_and(|tags| tags.iter().any(|value| value.as_str() == Some(tag)))
    };
    if entry["type"] != "language" || !tagged("tool-use") || !tagged("reasoning") {
        return None;
    }
    let price = &entry["pricing"];
    let upper = |key: &str| {
        price[key].as_array()?.iter().find(|tier| tier["min"].as_u64().is_some_and(|min| min > 0))
    };
    let long_rate = |key: &str, base: f64| {
        upper(key).and_then(|tier| per_million(&tier["cost"])).unwrap_or(base)
    };
    let keys = ["input", "input_cache_read", "input_cache_write", "output"];
    let pricing = read_rates(price, keys, None).map(|base| Pricing {
        base,
        long: Rates {
            input: long_rate("input_tiers", base.input),
            cached: long_rate("input_cache_read_tiers", base.cached),
            cache_write: long_rate("input_cache_write_tiers", base.cache_write),
            output: long_rate("output_tiers", base.output),
        },
    });
    let efforts = entry["reasoning_options"]
        .as_array()
        .and_then(|options| options.iter().find(|option| option["type"] == "effort"))
        .map_or_else(Vec::new, |option| named_efforts(&option["values"]));
    Some(Model {
        id: entry["id"].as_str()?.to_owned(),
        window: entry["context_window"].as_u64().unwrap_or(DEFAULT_WINDOW),
        efforts,
        tier: upper("input_tiers").and_then(|tier| tier["min"].as_u64()),
        pricing,
        images: accepts_images(&entry["modalities"]["input"]),
    })
}

/// Whether a listing's input modalities include images. A listing that names
/// none says nothing against them.
fn accepts_images(modalities: &Value) -> bool {
    modalities
        .as_array()
        .is_none_or(|inputs| inputs.iter().any(|input| input.as_str() == Some("image")))
}

/// Rates from per-token prices named by `keys` (input, cached, cache write,
/// output). Missing prices come from `fallback`; cache prices otherwise
/// default to the input price.
fn read_rates(prices: &Value, keys: [&str; 4], fallback: Option<Rates>) -> Option<Rates> {
    let [input, cached, cache_write, output] = keys.map(|key| per_million(&prices[key]));
    let input = input.or(fallback.map(|rates| rates.input))?;
    Some(Rates {
        input,
        cached: cached.or(fallback.map(|rates| rates.cached)).unwrap_or(input),
        cache_write: cache_write.or(fallback.map(|rates| rates.cache_write)).unwrap_or(input),
        output: output.or(fallback.map(|rates| rates.output))?,
    })
}

/// A dollars-per-token price, written as a string or a number, in dollars per
/// million tokens. Negative prices mark variable pricing agt cannot estimate.
fn per_million(value: &Value) -> Option<f64> {
    let price =
        value.as_str().and_then(|text| text.parse::<f64>().ok()).or_else(|| value.as_f64())?;
    // Rounding keeps prices such as 0.00001 exact after scaling.
    (price.is_finite() && price >= 0.0).then(|| (price * 1e10).round() / 1e4)
}

/// The known efforts a JSON array names, least first.
fn named_efforts(values: &Value) -> Vec<Effort> {
    let names = values.as_array().map_or(&[][..], Vec::as_slice);
    Effort::ALL
        .into_iter()
        .filter(|effort| names.iter().any(|name| name.as_str() == Some(effort.as_str())))
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// A listing as a provider sends it.
    fn read(listing: &str) -> Value {
        serde_json::from_str(listing).expect("listing is JSON")
    }

    fn usage(input: u64, cached: u64, output: u64) -> Usage {
        Usage { input, output, cached, cache_write: 0, cost: None }
    }

    #[test]
    fn openrouter_listings_keep_tool_calling_reasoning_models() {
        // Entries as OpenRouter lists them, trimmed to the fields agt reads.
        let listing = r#"{"data":[
            {"id":"openai/gpt-6-astra","context_length":1050000,"architecture":{"input_modalities":["text","image","file"]},
             "pricing":{"prompt":"0.00001","completion":"0.00005","input_cache_read":"0.000001","input_cache_write":"0.0000125",
                "overrides":[{"min_prompt_tokens":272000,"prompt":"0.00002","completion":"0.000075","input_cache_read":"0.000002","input_cache_write":"0.000025"}]},
             "supported_parameters":["include_reasoning","reasoning","tool_choice","tools"],
             "reasoning":{"supported_efforts":["max","xhigh","high","medium","low"]}},
            {"id":"google/gemini-3.8-flash","context_length":1048576,
             "pricing":{"prompt":"0.00000075","completion":"0.00000375","input_cache_read":"0.000000075"},
             "supported_parameters":["reasoning","tools"],"reasoning":{"supported_efforts":["high","medium","low"]}},
            {"id":"openrouter/auto","pricing":{"prompt":"-1","completion":"-1"},"supported_parameters":["reasoning","tools"]},
            {"id":"openai/gpt-6-astra:batch","supported_parameters":["reasoning","tools"]},
            {"id":"z-ai/glm-5.3","context_length":200000,"architecture":{"input_modalities":["text"]},"supported_parameters":["reasoning","tools"]},
            {"id":"meta/chat","context_length":8192,"supported_parameters":["tools"]}
        ]}"#;
        let models = parse(&read(listing), openrouter).expect("listing");
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(
            ids,
            ["google/gemini-3.8-flash", "openai/gpt-6-astra", "openrouter/auto", "z-ai/glm-5.3"]
        );
        assert!(models[1].images && models[2].images, "listed or unknown");
        assert!(!models[3].images, "text only");
        let astra = &models[1];
        assert_eq!(astra.window, 1_050_000);
        assert_eq!(astra.efforts, GPT_6_EFFORTS);
        assert_eq!(astra.tier, Some(272_000));
        assert_eq!(
            astra.pricing,
            Some(Pricing {
                base: rates(10.0, 1.0, 12.5, 50.0),
                long: rates(20.0, 2.0, 25.0, 75.0)
            })
        );
        let gemini = models[0].pricing.expect("prices");
        assert_eq!(gemini.base, rates(0.75, 0.075, 0.75, 3.75));
        assert_eq!(gemini.long, gemini.base);
        assert_eq!(models[0].tier, None);
        assert_eq!(models[2].pricing, None, "variable prices are unknown");
        assert_eq!(models[2].window, DEFAULT_WINDOW);
    }

    #[test]
    fn vercel_listings_read_tiers_and_effort_options() {
        let listing = r#"{"data":[
            {"id":"openai/gpt-5.6-luna","type":"language","context_window":1050000,"modalities":{"input":["text","image","pdf"]},
             "tags":["reasoning","tool-use","implicit-caching"],
             "reasoning_options":[{"type":"effort","values":["none","low","medium","high","xhigh","max"]}],
             "pricing":{"input":"0.0000002","output":"0.0000012","input_cache_read":"0.00000002","input_cache_write":"0.00000025",
                "input_tiers":[{"cost":"0.0000002","min":0,"max":272000},{"cost":"0.0000004","min":272000}],
                "output_tiers":[{"cost":"0.0000012","min":0,"max":272000},{"cost":"0.0000018","min":272000}],
                "input_cache_read_tiers":[{"cost":"0.00000002","max":272000},{"cost":"0.00000004","min":272000}],
                "input_cache_write_tiers":[{"cost":"0.00000025","min":0,"max":272000},{"cost":"0.0000005","min":272000}]}},
            {"id":"anthropic/claude-opus-5","type":"language","context_window":1000000,"tags":["tool-use","reasoning"],
             "reasoning_options":[{"type":"budget_tokens"}],"pricing":{"input":"0.000005","output":"0.000025"}},
            {"id":"openai/text-embedding-3-small","type":"embedding","tags":["reasoning","tool-use"]},
            {"id":"openai/gpt-4o","type":"language","tags":["tool-use"]},
            {"id":"alibaba/qwen3.7-max","type":"language","context_window":262144,"tags":["reasoning","tool-use"],"modalities":{"input":["text"]}}
        ]}"#;
        let models = parse(&read(listing), vercel).expect("listing");
        assert_eq!(models.len(), 3);
        let (qwen, opus, luna) = (&models[0], &models[1], &models[2]);
        assert!(luna.images && opus.images);
        assert!(!qwen.images);
        assert_eq!(luna.efforts, GPT_5_6_EFFORTS);
        assert_eq!(luna.tier, Some(272_000));
        assert_eq!(
            luna.pricing,
            Some(Pricing { base: rates(0.2, 0.02, 0.25, 1.2), long: rates(0.4, 0.04, 0.5, 1.8) })
        );
        assert!(opus.efforts.is_empty());
        assert_eq!(opus.pricing.expect("prices").base, rates(5.0, 5.0, 5.0, 25.0));
        assert!(parse(&json!({ "models": [] }), openrouter).is_err());
        assert!(parse(&json!({ "data": {} }), vercel).is_err());
    }

    #[test]
    fn costs_follow_cache_reads_and_long_context_rates() {
        let astra = find(Provider::OpenAi, "gpt-6-astra");
        let cost = astra.cost(&usage(100_000, 80_000, 1_000)).expect("priced");
        assert!((cost - 0.33).abs() < 1e-9, "{cost}");
        let long = astra.cost(&usage(300_000, 0, 0)).expect("priced");
        assert!((long - 6.0).abs() < 1e-9, "{long}");
        let mut written = usage(10_000, 0, 0);
        written.cache_write = 10_000;
        let write = astra.cost(&written).expect("priced");
        assert!((write - 0.125).abs() < 1e-9, "{write}");
        let chatgpt = find(Provider::Codex, "gpt-6-astra");
        assert_eq!(chatgpt.cost(&usage(100, 0, 10)), None, "subscriptions have no token prices");
    }

    #[test]
    fn known_catalogs_give_each_providers_window_tier_and_efforts() {
        let astra = find(Provider::OpenAi, "gpt-6-astra");
        assert_eq!((astra.window, astra.tier), (1_050_000, Some(272_000)));
        assert_eq!(efforts(Some(&astra)), GPT_6_EFFORTS);
        let chatgpt = find(Provider::Codex, "gpt-6-astra");
        assert_eq!((chatgpt.window, chatgpt.tier), (272_000, Some(272_000)));
        // As xAI's model listing and the Grok CLI's catalog describe them.
        let grok = find(Provider::Grok, "grok-4.6");
        assert_eq!((grok.window, grok.tier, grok.pricing), (500_000, Some(200_000), None));
        assert_eq!(
            efforts(Some(&grok)),
            [Effort::Low, Effort::Medium, Effort::High, Effort::Xhigh]
        );
        let grok = find(Provider::Grok, "grok-4.5");
        assert_eq!(grok.efforts, [Effort::Low, Effort::Medium, Effort::High]);
        let unknown = find(Provider::OpenAi, "gpt-7");
        let defaults = (unknown.window, efforts(Some(&unknown)));
        assert_eq!(defaults, (DEFAULT_WINDOW, &Effort::ALL[..]), "an unknown model gets defaults");
    }

    #[test]
    fn saved_models_round_trip() {
        for model in [find(Provider::OpenAi, "gpt-5.6-sol"), find(Provider::Codex, "gpt-5.6-sol")] {
            assert_eq!(
                Model::from_json(&serde_json::to_value(&model).expect("model")),
                Some(model)
            );
        }
        assert_eq!(Model::from_json(&json!({ "id": " " })), None);
        let bare = Model::from_json(&json!({ "id": "m", "efforts": ["high", "bogus"] }));
        assert_eq!(bare, Some(Model { efforts: vec![Effort::High], ..Model::unknown("m") }));
    }
}
