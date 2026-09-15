//! The five providers and everything that differs between them: endpoints,
//! sign-in, request dialects, native compaction and model catalogs.
//!
//! Each provider is one entry here, and the rest of agt reads these entries
//! rather than naming providers, so adding or changing a provider happens in
//! this file.

use serde_json::Value;

use crate::models::{self, Known, Model};

/// A provider of Responses API models.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Provider {
    OpenAi,
    /// ChatGPT subscriptions, through the backend Codex uses.
    Codex,
    /// SuperGrok and X Premium+ subscriptions, through xAI's API.
    Grok,
    OpenRouter,
    Vercel,
}

impl Provider {
    pub(crate) const ALL: [Self; 5] =
        [Self::OpenAi, Self::Codex, Self::Grok, Self::OpenRouter, Self::Vercel];

    pub(crate) fn spec(self) -> &'static Spec {
        match self {
            Self::OpenAi => &OPENAI,
            Self::Codex => &CODEX,
            Self::Grok => &GROK,
            Self::OpenRouter => &OPENROUTER,
            Self::Vercel => &VERCEL,
        }
    }

    /// The provider with id `id`, as `AGT_PROVIDER` and saved settings name it.
    pub(crate) fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|provider| provider.spec().id == id)
    }
}

/// What agt needs to know about a provider.
pub(crate) struct Spec {
    /// Names the provider in settings, credentials, logs and `AGT_PROVIDER`.
    pub(crate) id: &'static str,
    pub(crate) name: &'static str,
    /// The base URL of its Responses API.
    pub(crate) url: &'static str,
    /// Describes the provider in the sign-in list until it has a credential.
    pub(crate) about: &'static str,
    pub(crate) access: Access,
    pub(crate) dialect: Dialect,
    /// How the provider compacts the models `native_models` names by id
    /// prefix; others it does not compact.
    pub(crate) native: Compaction,
    pub(crate) native_models: &'static str,
    pub(crate) catalog: Catalog,
}

impl Spec {
    /// How the provider compacts conversations with `model` itself.
    pub(crate) fn compaction(&self, model: &str) -> Compaction {
        if model.starts_with(self.native_models) { self.native } else { Compaction::None }
    }
}

/// How a provider compacts a model's context itself, if it does.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Compaction {
    /// It does not, so agt elides old output and writes a summary.
    None,
    /// `POST /responses/compact` returns the next context, used as returned.
    Endpoint,
    /// Requests carry `context_management`, so the provider compacts while it
    /// responds; the endpoint serves compaction the user asks for.
    Inline,
}

/// How requests to a provider authenticate.
pub(crate) enum Access {
    /// An API key: pasted, read from `env`, or issued by a browser `sign_in`.
    Key { env: &'static str, sign_in: Option<KeySignIn> },
    /// A subscription signed in with OAuth, whose tokens refresh.
    Subscription(OAuth),
}

/// A browser sign-in with PKCE that issues an ordinary API key.
pub(crate) struct KeySignIn {
    pub(crate) authorize: &'static str,
    /// Where the code is exchanged for the key.
    pub(crate) keys: &'static str,
}

/// An OAuth authorization code sign-in with PKCE.
pub(crate) struct OAuth {
    pub(crate) authorize: &'static str,
    /// Where codes are exchanged and tokens refreshed.
    pub(crate) token: &'static str,
    pub(crate) client: &'static str,
    pub(crate) scope: &'static str,
    pub(crate) redirect: Redirect,
    /// Query parameters the sign-in page takes beyond the standard ones.
    pub(crate) params: &'static [(&'static str, &'static str)],
    /// The account access tokens name, which requests carry.
    pub(crate) account: Option<Account>,
}

/// Where the browser returns after signing in.
#[derive(Clone, Copy)]
pub(crate) struct Redirect {
    pub(crate) host: &'static str,
    /// The port a client is registered with, or 0 for any free port.
    pub(crate) port: u16,
    pub(crate) path: &'static str,
}

impl Redirect {
    /// A loopback redirect to any free port.
    pub(crate) const LOOPBACK: Self = Self { host: "127.0.0.1", port: 0, path: "/callback" };

    /// The redirect's address once agt listens on `port`.
    pub(crate) fn url(self, port: u16) -> String {
        format!("http://{}:{port}{}", self.host, self.path)
    }
}

/// An account that access tokens name.
pub(crate) struct Account {
    /// A JSON pointer to the account id among the token's claims.
    pub(crate) claim: &'static str,
    /// The header that carries the id.
    pub(crate) header: &'static str,
}

/// What a provider's Responses API takes beyond the fields every provider
/// shares, as each provider documents it, stated as how it differs from
/// OpenAI.
#[derive(Clone, Copy)]
pub(crate) struct Dialect {
    /// Headers every request carries.
    pub(crate) headers: &'static [(&'static str, &'static str)],
    /// The header carrying the session id that keeps a conversation on the
    /// backend holding its prompt cache.
    pub(crate) affinity: Option<&'static str>,
    /// Whether requests carry `prompt_cache_key`.
    pub(crate) cache_key: bool,
    /// How requests ask for prompt caching, which Anthropic's models do only
    /// when asked.
    pub(crate) caching: Caching,
    /// Whether requests carry `max_output_tokens`.
    pub(crate) output_limit: bool,
    /// Whether images stay inside the tool outputs that show them, rather than
    /// following a run of outputs in a user message.
    pub(crate) tool_images: bool,
}

#[derive(Clone, Copy)]
pub(crate) enum Caching {
    /// The provider caches without being asked.
    Implicit,
    /// A top-level `cache_control`.
    CacheControl,
    /// `caching` and `cache_ttl`.
    Gateway,
}

/// Where a provider's models come from.
pub(crate) enum Catalog {
    /// Models agt knows itself, with the provider's context window and the
    /// input size above which a whole request is billed at higher rates.
    Known { models: &'static [Known], window: u64, tier: u64 },
    /// Models the provider lists at `GET /models` without a key, each entry
    /// read by the function.
    Listed(fn(&Value) -> Option<Model>),
}

const OPENAI: Spec = Spec {
    id: "openai",
    name: "OpenAI",
    url: "https://api.openai.com/v1",
    about: "needs an API key",
    access: Access::Key { env: "OPENAI_API_KEY", sign_in: None },
    dialect: Dialect {
        headers: &[],
        affinity: None,
        cache_key: true,
        caching: Caching::Implicit,
        output_limit: true,
        tool_images: true,
    },
    native: Compaction::Inline,
    native_models: "",
    // GPT-5.6 and later bill a request with more input tokens than the tier
    // at higher rates, and ChatGPT plans count such requests as heavier use.
    catalog: Catalog::Known { models: &models::OPENAI, window: 1_050_000, tier: 272_000 },
};

const CODEX: Spec = Spec {
    id: "codex",
    name: "ChatGPT",
    url: "https://chatgpt.com/backend-api/codex",
    about: "ChatGPT Plus or Pro subscription",
    access: Access::Subscription(OAuth {
        authorize: "https://auth.openai.com/oauth/authorize",
        token: "https://auth.openai.com/oauth/token",
        // OpenAI's Codex CLI client.
        client: "app_EMoamEEZ73f0CkXaXp7hrann",
        scope: "openid profile email offline_access",
        // Codex clients are registered with this redirect, so its port is fixed.
        redirect: Redirect { host: "localhost", port: 1455, path: "/auth/callback" },
        params: &[
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("originator", "agt"),
        ],
        account: Some(Account {
            claim: "/https:~1~1api.openai.com~1auth/chatgpt_account_id",
            header: "chatgpt-account-id",
        }),
    }),
    // The ChatGPT backend takes no output limit.
    dialect: Dialect {
        headers: &[("openai-beta", "responses=experimental"), ("originator", "agt")],
        affinity: Some("session-id"),
        output_limit: false,
        ..OPENAI.dialect
    },
    // Whether the ChatGPT backend takes `context_management` is not known.
    native: Compaction::Endpoint,
    native_models: "",
    // The window the ChatGPT backend serves to Codex clients.
    catalog: Catalog::Known { models: &models::OPENAI, window: 272_000, tier: 272_000 },
};

const GROK: Spec = Spec {
    id: "grok",
    name: "Grok",
    url: "https://api.x.ai/v1",
    about: "SuperGrok or X Premium+ subscription",
    access: Access::Subscription(OAuth {
        authorize: "https://auth.x.ai/oauth2/authorize",
        token: "https://auth.x.ai/oauth2/token",
        // xAI's Grok CLI client, which takes a redirect to any loopback port.
        client: "b1a00492-073a-47ea-816f-4c329264a828",
        scope: "openid profile email offline_access grok-cli:access api:access",
        redirect: Redirect::LOOPBACK,
        params: &[("referrer", "agt")],
        account: None,
    }),
    dialect: Dialect { affinity: Some("x-grok-conv-id"), ..OPENAI.dialect },
    // xAI documents `/responses/compact` alone.
    native: Compaction::Endpoint,
    native_models: "",
    // xAI bills a request with more input tokens than the tier at twice the
    // rates. It lists models only to signed-in clients and mixes in image and
    // video models, so the ones a subscription offers for coding are known.
    catalog: Catalog::Known { models: &models::GROK, window: 500_000, tier: 200_000 },
};

const OPENROUTER: Spec = Spec {
    id: "openrouter",
    name: "OpenRouter",
    url: "https://openrouter.ai/api/v1",
    about: "sign in or paste an API key",
    access: Access::Key {
        env: "OPENROUTER_API_KEY",
        sign_in: Some(KeySignIn {
            authorize: "https://openrouter.ai/auth",
            keys: "https://openrouter.ai/api/v1/auth/keys",
        }),
    },
    // The gateways convert requests for other providers, and OpenRouter's
    // conversion for Gemini on Vertex drops a tool output holding an image,
    // so they get the images of a run of outputs in a user message after it,
    // as Chat Completions clients send them. OpenRouter routes its cache by
    // the affinity header alone.
    dialect: Dialect {
        affinity: Some("x-session-id"),
        cache_key: false,
        caching: Caching::CacheControl,
        tool_images: false,
        ..OPENAI.dialect
    },
    // OpenRouter compacts nothing itself.
    native: Compaction::None,
    native_models: "",
    catalog: Catalog::Listed(models::openrouter),
};

const VERCEL: Spec = Spec {
    id: "vercel",
    name: "Vercel AI Gateway",
    url: "https://ai-gateway.vercel.sh/v1",
    about: "needs an API key",
    access: Access::Key { env: "AI_GATEWAY_API_KEY", sign_in: None },
    dialect: Dialect {
        affinity: Some("x-session-affinity"),
        caching: Caching::Gateway,
        tool_images: false,
        ..OPENAI.dialect
    },
    // The gateway forwards compaction to OpenAI, for OpenAI's models only.
    native: Compaction::Endpoint,
    native_models: "openai/",
    catalog: Catalog::Listed(models::vercel),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_compaction_serves_the_models_each_provider_documents() {
        let compaction = |provider: Provider, model| provider.spec().compaction(model);
        assert_eq!(compaction(Provider::OpenAi, "gpt-6-astra"), Compaction::Inline);
        assert_eq!(compaction(Provider::Codex, "gpt-5.6-sol"), Compaction::Endpoint);
        assert_eq!(compaction(Provider::Grok, "grok-4.6"), Compaction::Endpoint);
        assert_eq!(compaction(Provider::Vercel, "openai/gpt-6-astra"), Compaction::Endpoint);
        assert_eq!(compaction(Provider::Vercel, "anthropic/claude-opus-5"), Compaction::None);
        assert_eq!(compaction(Provider::OpenRouter, "openai/gpt-6-astra"), Compaction::None);
    }
}
