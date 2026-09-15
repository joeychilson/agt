//! Providers and settings.
//!
//! agt works with five providers. The model chosen in the terminal UI is saved
//! with what agt knows about it in `$AGT_HOME/config.json`, and sign-ins and API
//! keys in `$AGT_HOME/auth.json`, which only the user can read. Environment
//! variables override both, so scripts, CI and editors can configure agt
//! without either file.

use std::fs::{self, File, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::auth::{Auth, Credential, Subscription, Tokens};
use crate::llm::Endpoint;
use crate::models::{self, Effort, MIN_WINDOW, Model};
use crate::provider::{Access, Compaction, Provider};

const SETTINGS: &str = "config.json";
const AUTH: &str = "auth.json";

/// Settings shared by every session.
pub(crate) struct Config {
    pub(crate) home: PathBuf,
    pub(crate) endpoint: Endpoint,
    /// `None` until a model is chosen.
    pub(crate) model: Option<Model>,
    pub(crate) reasoning: Option<Effort>,
    /// `AGT_CONTEXT_WINDOW`, which applies to every model.
    window_override: Option<u64>,
}

/// Choices made on the command line, which come before the environment and
/// the saved settings.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Overrides {
    pub(crate) provider: Option<Provider>,
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<Effort>,
}

/// What a session's requests use: the endpoint, the model and its effort.
#[derive(Clone)]
pub(crate) struct Settings {
    pub(crate) endpoint: Endpoint,
    pub(crate) model: Model,
    /// The reasoning effort, or `None` for the provider's default.
    pub(crate) reasoning: Option<Effort>,
    /// `AGT_CONTEXT_WINDOW`, which applies to every model.
    pub(crate) window_override: Option<u64>,
}

impl Config {
    /// Reads the settings in `home`, then applies the environment and the
    /// command line's `overrides`.
    pub(crate) fn load(home: PathBuf, overrides: Overrides) -> Result<Self, String> {
        let settings = read(&home, SETTINGS)?;
        let auth = read(&home, AUTH)?;
        resolve(home, &settings, &auth, overrides, env_var)
    }

    /// The settings a session starts with, once a model is chosen.
    pub(crate) fn settings(&self) -> Option<Settings> {
        Some(Settings {
            endpoint: self.endpoint.clone(),
            model: self.model.clone()?,
            reasoning: self.reasoning,
            window_override: self.window_override,
        })
    }

    /// Switches to `provider` with the credential it has now.
    pub(crate) fn set_provider(&mut self, provider: Provider) -> Result<(), String> {
        let auth = read(&self.home, AUTH)?;
        let auth = credential(&self.home, provider, &auth, env_var);
        self.endpoint = Endpoint { provider, url: base_url(provider), auth };
        Ok(())
    }
}

impl Settings {
    /// The context window: `AGT_CONTEXT_WINDOW`, else the model's.
    pub(crate) fn window(&self) -> u64 {
        self.window_override.unwrap_or(self.model.window)
    }

    /// How the provider compacts conversations with the model itself.
    pub(crate) fn compaction(&self) -> Compaction {
        self.endpoint.provider.spec().compaction(&self.model.id)
    }
}

/// The agt home directory: `AGT_HOME`, or `~/.agt`.
pub(crate) fn home() -> Result<PathBuf, String> {
    env_var("AGT_HOME")
        .map(PathBuf::from)
        .or_else(|| env_var("HOME").map(|home| Path::new(&home).join(".agt")))
        .ok_or_else(|| "set AGT_HOME or HOME".into())
}

/// The endpoint of `provider`, unless `AGT_BASE_URL` replaces it.
pub(crate) fn base_url(provider: Provider) -> String {
    env_var("AGT_BASE_URL").unwrap_or_else(|| provider.spec().url.to_owned())
}

/// Whether `provider` has a credential, saved or in the environment.
pub(crate) fn signed_in(home: &Path, provider: Provider) -> bool {
    read(home, AUTH)
        .is_ok_and(|auth| !matches!(credential(home, provider, &auth, env_var), Auth::None))
}

/// The providers whose models can be used: `current` and those with a
/// credential.
pub(crate) fn usable_providers(home: &Path, current: Provider) -> Vec<Provider> {
    Provider::ALL
        .into_iter()
        .filter(|&provider| provider == current || signed_in(home, provider))
        .collect()
}

/// The provider `id` names, where `source` is the option or variable that
/// gave it.
pub(crate) fn provider(source: &str, id: &str) -> Result<Provider, String> {
    Provider::from_id(id.trim()).ok_or_else(|| {
        let ids: Vec<&str> = Provider::ALL.iter().map(|known| known.spec().id).collect();
        format!("{source} must be one of {}, not {id:?}", ids.join(", "))
    })
}

/// The reasoning effort `name` names, where `source` is the option or
/// variable that gave it.
pub(crate) fn effort(source: &str, name: &str) -> Result<Effort, String> {
    Effort::parse(name.trim()).ok_or_else(|| {
        let names: Vec<&str> = Effort::ALL.iter().map(|effort| effort.as_str()).collect();
        format!("{source} must be one of {}, not {name:?}", names.join(", "))
    })
}

/// The sign-in to `provider` saved in `auth.json`, which another process may
/// have refreshed.
pub(crate) fn saved_tokens(home: &Path, provider: Provider) -> Option<Tokens> {
    Tokens::deserialize(read(home, AUTH).ok()?.get(provider.spec().id)?).ok()
}

/// Locks the sign-ins in `home` against other agt processes until the
/// returned file is dropped.
pub(crate) fn lock_credentials(home: &Path) -> io::Result<File> {
    fs::create_dir_all(home)?;
    let file =
        File::options().write(true).create(true).truncate(false).open(home.join("auth.lock"))?;
    file.lock()?;
    Ok(file)
}

/// Saves the credential for `provider`.
pub(crate) fn save_credential(
    home: &Path,
    provider: Provider,
    credential: &Credential,
) -> io::Result<()> {
    let value = serde_json::to_value(credential).map_err(io::Error::other)?;
    update(home, AUTH, true, |auth| {
        auth.insert(provider.spec().id.to_owned(), value);
    })
}

/// Forgets the credential saved for `provider`. Returns false when none was
/// saved.
pub(crate) fn remove_credential(home: &Path, provider: Provider) -> io::Result<bool> {
    let mut removed = false;
    update(home, AUTH, true, |auth| removed = auth.remove(provider.spec().id).is_some())?;
    Ok(removed)
}

/// Saves `model` on `provider` as the model to use.
pub(crate) fn save_model(home: &Path, provider: Provider, model: &Model) -> io::Result<()> {
    let mut value = serde_json::to_value(model).map_err(io::Error::other)?;
    value["provider"] = provider.spec().id.into();
    save(home, "model", Some(value))
}

/// Saves the reasoning effort; `None` leaves it to the provider.
pub(crate) fn save_effort(home: &Path, effort: Option<Effort>) -> io::Result<()> {
    save(home, "reasoning", effort.map(|effort| effort.as_str().into()))
}

/// Sets a field in the settings file, or removes it when `value` is `None`.
/// Fields agt does not use are kept.
fn save(home: &Path, name: &str, value: Option<Value>) -> io::Result<()> {
    update(home, SETTINGS, false, |settings| {
        match value {
            Some(value) => settings.insert(name.to_owned(), value),
            None => settings.remove(name),
        };
    })
}

/// The configuration from saved settings and credentials, the command line's
/// `overrides` and the environment variables `env` reads.
pub(crate) fn resolve(
    home: PathBuf,
    settings: &Map<String, Value>,
    auth: &Map<String, Value>,
    overrides: Overrides,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Config, String> {
    let saved = settings.get("model").filter(|model| model.is_object());
    let saved_provider =
        saved.and_then(|model| model["provider"].as_str()).and_then(Provider::from_id);
    let provider = match (overrides.provider, env("AGT_PROVIDER")) {
        (Some(provider), _) => provider,
        (None, Some(id)) => provider("AGT_PROVIDER", &id)?,
        (None, None) => saved_provider.unwrap_or(Provider::OpenAi),
    };
    let window_override =
        env("AGT_CONTEXT_WINDOW")
            .map(|value| {
                value.trim().parse::<u64>().ok().filter(|window| *window >= MIN_WINDOW).ok_or_else(
                    || format!("AGT_CONTEXT_WINDOW must be at least {MIN_WINDOW} tokens"),
                )
            })
            .transpose()?;
    // A saved model belongs to the provider it was chosen on.
    let saved = saved.filter(|_| saved_provider == Some(provider)).and_then(Model::from_json);
    let model = match overrides.model.or_else(|| env("AGT_MODEL")) {
        Some(id) => Some(
            saved.filter(|model| model.id == id).unwrap_or_else(|| models::find(provider, &id)),
        ),
        None => saved,
    };
    let reasoning = match (overrides.effort, env("AGT_REASONING")) {
        (Some(effort), _) => Some(effort),
        (None, Some(name)) => Some(effort("AGT_REASONING", &name)?),
        // A saved effort agt does not know leaves the effort to the provider.
        (None, None) => settings.get("reasoning").and_then(Value::as_str).and_then(Effort::parse),
    };
    let auth = credential(&home, provider, auth, &env);
    let url = env("AGT_BASE_URL").unwrap_or_else(|| provider.spec().url.to_owned());
    Ok(Config {
        home,
        endpoint: Endpoint { provider, url, auth },
        model,
        reasoning,
        window_override,
    })
}

/// The credential requests to `provider` use. When `AGT_BASE_URL` names the
/// endpoint only `AGT_API_KEY` is sent, so a saved key never reaches an
/// endpoint the environment chose.
fn credential(
    home: &Path,
    provider: Provider,
    auth: &Map<String, Value>,
    env: impl Fn(&str) -> Option<String>,
) -> Auth {
    if let Some(key) = env("AGT_API_KEY") {
        return Auth::Key(key);
    }
    if env("AGT_BASE_URL").is_some() {
        return Auth::None;
    }
    let saved = auth.get(provider.spec().id).and_then(|saved| Credential::deserialize(saved).ok());
    match (&provider.spec().access, saved) {
        (Access::Subscription(_), Some(Credential::Tokens(tokens))) => {
            Subscription::new(provider, tokens, home.to_path_buf())
                .map_or(Auth::None, |subscription| Auth::Subscription(Arc::new(subscription)))
        }
        (Access::Subscription(_), _) => Auth::None,
        (Access::Key { .. }, Some(Credential::Key(key))) if !key.trim().is_empty() => {
            Auth::Key(key.trim().to_owned())
        }
        (Access::Key { env: variable, .. }, _) => env(variable).map_or(Auth::None, Auth::Key),
    }
}

fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.trim().is_empty())
}

/// Reads a JSON object from a file in `home`; a missing file is empty.
fn read(home: &Path, name: &str) -> Result<Map<String, Value>, String> {
    let path = home.join(name);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    match serde_json::from_slice(&bytes) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(format!("{}: expected a JSON object", path.display())),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

/// Rewrites a file in `home`. The new contents are written beside it and
/// renamed over it, so a crash never leaves a partial file.
pub(crate) fn update(
    home: &Path,
    name: &str,
    private: bool,
    change: impl FnOnce(&mut Map<String, Value>),
) -> io::Result<()> {
    // A request thread refreshing a sign-in and the terminal UI can save at
    // once; each reads what the other wrote, and they never share the
    // temporary file.
    static WRITING: Mutex<()> = Mutex::new(());
    let _writing = WRITING.lock().unwrap_or_else(PoisonError::into_inner);
    let mut map = read(home, name).map_err(io::Error::other)?;
    change(&mut map);
    let mut bytes = serde_json::to_vec_pretty(&map).map_err(io::Error::other)?;
    bytes.push(b'\n');
    fs::create_dir_all(home)?;
    // The process id keeps concurrent agt processes from sharing a file.
    let temp = home.join(format!(".{name}.{}", std::process::id()));
    let result =
        write_new(&temp, &bytes, private).and_then(|()| fs::rename(&temp, home.join(name)));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn write_new(path: &Path, bytes: &[u8], private: bool) -> io::Result<()> {
    let mut file = File::create(path)?;
    if private {
        // Restricted before anything secret is written.
        file.set_permissions(Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn resolved(
        settings: Value,
        auth: Value,
        model: Option<&str>,
        env: &[(&str, &str)],
    ) -> Result<Config, String> {
        let overrides = Overrides { model: model.map(str::to_owned), ..Overrides::default() };
        resolved_with(settings, auth, overrides, env)
    }

    fn resolved_with(
        settings: Value,
        auth: Value,
        overrides: Overrides,
        env: &[(&str, &str)],
    ) -> Result<Config, String> {
        let object = |value: Value| match value {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        let lookup = |name: &str| {
            env.iter().find(|(key, _)| *key == name).map(|(_, value)| (*value).to_owned())
        };
        resolve(PathBuf::from("/home/.agt"), &object(settings), &object(auth), overrides, lookup)
    }

    fn key(config: &Config) -> Option<&str> {
        match &config.endpoint.auth {
            Auth::Key(key) => Some(key),
            _ => None,
        }
    }

    #[test]
    fn defaults_to_openai_with_its_usual_key() {
        let config =
            resolved(json!({}), json!({}), None, &[("OPENAI_API_KEY", "sk-env")]).expect("valid");
        assert_eq!(config.endpoint.provider, Provider::OpenAi);
        assert_eq!(config.endpoint.url, "https://api.openai.com/v1");
        assert_eq!(key(&config), Some("sk-env"));
        assert_eq!(config.model, None);
        assert_eq!(config.reasoning, None);
    }

    /// Claude Opus 5, and settings that saved it on OpenRouter at high effort.
    fn saved_opus() -> (Model, Value) {
        let opus = Model {
            id: "anthropic/claude-opus-5".into(),
            window: 1_000_000,
            efforts: vec![Effort::High],
            tier: None,
            pricing: None,
            images: true,
        };
        let mut model = serde_json::to_value(&opus).expect("model");
        model["provider"] = "openrouter".into();
        (opus, json!({ "model": model, "reasoning": "high" }))
    }

    #[test]
    fn saved_settings_choose_the_provider_model_and_key() {
        let (opus, settings) = saved_opus();
        let env = [("OPENROUTER_API_KEY", "sk-env")];
        let auth = json!({ "openrouter": " sk-saved " });
        let config = resolved(settings.clone(), auth, None, &env).expect("valid");
        assert_eq!(config.endpoint.provider, Provider::OpenRouter);
        assert_eq!(config.endpoint.url, "https://openrouter.ai/api/v1");
        assert_eq!(key(&config), Some("sk-saved"), "a saved key comes before the variable");
        assert_eq!(config.model, Some(opus));
        assert_eq!(config.reasoning, Some(Effort::High));
        let config = resolved(settings, json!({}), None, &env).expect("valid");
        assert_eq!(key(&config), Some("sk-env"));
        // A model saved without its details is not used.
        let bare = json!({ "provider": "openrouter", "model": "m1" });
        assert_eq!(resolved(bare, json!({}), None, &[]).expect("valid").model, None);
    }

    #[test]
    fn the_environment_overrides_saved_settings() {
        let (opus, settings) = saved_opus();
        let auth = json!({ "openrouter": "sk-saved" });
        let with = |model: Option<&str>, env: &[(&str, &str)]| {
            resolved(settings.clone(), auth.clone(), model, env).expect("valid")
        };
        let other = with(None, &[("AGT_MODEL", "m2")]).model.expect("model");
        let details = (other.id.as_str(), other.window);
        assert_eq!(details, ("m2", 200_000), "another model inherits nothing from the saved one");
        let chosen = with(Some("anthropic/claude-opus-5"), &[("AGT_MODEL", "m2")]).model;
        assert_eq!(chosen, Some(opus), "--model comes before AGT_MODEL");

        let config = with(None, &[("AGT_PROVIDER", "openai")]);
        assert_eq!(config.endpoint.provider, Provider::OpenAi);
        let saved = (key(&config), config.model.as_ref());
        assert_eq!(saved, (None, None), "keys and models belong to their provider");

        let env =
            [("AGT_API_KEY", "sk-1"), ("AGT_CONTEXT_WINDOW", "32000"), ("AGT_REASONING", "low")];
        let config = with(None, &env);
        assert_eq!(key(&config), Some("sk-1"));
        assert_eq!(config.settings().expect("settings").window(), 32_000);
        assert_eq!(config.reasoning, Some(Effort::Low));

        // Options come before the variables.
        let overrides = Overrides {
            provider: Some(Provider::Grok),
            model: Some("grok-4.6".into()),
            effort: Some(Effort::Xhigh),
        };
        let env =
            [("AGT_PROVIDER", "openai"), ("AGT_MODEL", "gpt-6-astra"), ("AGT_REASONING", "low")];
        let config = resolved_with(settings.clone(), auth.clone(), overrides, &env).expect("valid");
        assert_eq!(config.endpoint.provider, Provider::Grok);
        assert_eq!(config.model.map(|model| model.id).as_deref(), Some("grok-4.6"));
        assert_eq!(config.reasoning, Some(Effort::Xhigh));
    }

    #[test]
    fn a_custom_base_url_is_sent_only_agt_api_key() {
        let (_, settings) = saved_opus();
        let auth = json!({ "openrouter": "sk-saved" });
        let url = ("AGT_BASE_URL", "http://localhost:8080/v1");
        let env = [url, ("OPENROUTER_API_KEY", "sk-env")];
        let config = resolved(settings.clone(), auth.clone(), None, &env).expect("valid");
        assert_eq!(config.endpoint.url, "http://localhost:8080/v1");
        assert!(
            matches!(config.endpoint.auth, Auth::None),
            "neither the saved key nor the provider's variable is sent"
        );
        let config =
            resolved(settings, auth, None, &[url, ("AGT_API_KEY", "sk-1")]).expect("valid");
        assert_eq!(key(&config), Some("sk-1"));
    }

    #[test]
    fn subscription_sign_ins_are_read_from_auth() {
        let tokens = json!({ "access": "a", "refresh": "r", "expires": 1, "account": "acct" });
        let auth = json!({ "codex": tokens, "grok": tokens, "openrouter": tokens });
        for (provider, url) in
            [("codex", "https://chatgpt.com/backend-api/codex"), ("grok", "https://api.x.ai/v1")]
        {
            let env = [("AGT_PROVIDER", provider)];
            let config = resolved(json!({}), auth.clone(), None, &env).expect("valid");
            assert_eq!(config.endpoint.url, url);
            assert!(matches!(config.endpoint.auth, Auth::Subscription(_)), "{provider}");
        }
        // Tokens are no credential for a provider that takes keys.
        let env = [("AGT_PROVIDER", "openrouter")];
        let config = resolved(json!({}), auth, None, &env).expect("valid");
        assert!(matches!(config.endpoint.auth, Auth::None));
    }

    #[test]
    fn invalid_settings_are_reported() {
        let error = |env: &[(&str, &str)]| resolved(json!({}), json!({}), None, env).err();
        assert_eq!(
            error(&[("AGT_PROVIDER", "custom")]).as_deref(),
            Some(
                r#"AGT_PROVIDER must be one of openai, codex, grok, openrouter, vercel, not "custom""#
            )
        );
        for window in ["100", "lots"] {
            let too_small = Some("AGT_CONTEXT_WINDOW must be at least 8000 tokens");
            assert_eq!(error(&[("AGT_CONTEXT_WINDOW", window)]).as_deref(), too_small, "{window}");
        }
        assert_eq!(
            error(&[("AGT_REASONING", "extreme")]).as_deref(),
            Some(
                r#"AGT_REASONING must be one of none, minimal, low, medium, high, xhigh, max, not "extreme""#
            )
        );
        let home = tempfile::tempdir().expect("temp dir");
        let path = home.path().join(SETTINGS);
        for contents in ["[1]", "{\"model\":"] {
            fs::write(&path, contents).expect("write");
            let error = Config::load(home.path().to_path_buf(), Overrides::default())
                .err()
                .expect("rejected");
            let named = error.starts_with(&path.display().to_string());
            assert!(named, "{contents}: the error does not name the file: {error}");
        }
    }

    #[test]
    fn concurrent_saves_keep_every_change() {
        let dir = tempfile::tempdir().expect("temp dir");
        let home = dir.path();
        std::thread::scope(|scope| {
            for thread in 0..4 {
                scope.spawn(move || {
                    for n in 0..10 {
                        save(home, &format!("t{thread}-{n}"), Some(n.into())).expect("save");
                    }
                });
            }
        });
        assert_eq!(read(home, SETTINGS).expect("read").len(), 40);
    }

    #[test]
    fn credential_locks_exclude_other_holders() {
        let home = tempfile::tempdir().expect("temp dir");
        let held = lock_credentials(home.path()).expect("lock");
        let other = File::open(home.path().join("auth.lock")).expect("lock file");
        assert!(other.try_lock().is_err());
        drop(held);
        assert!(other.try_lock().is_ok());
    }

    #[test]
    fn saving_settings_keeps_fields_agt_does_not_use() {
        let dir = tempfile::tempdir().expect("temp dir");
        let home = dir.path();
        fs::write(home.join(SETTINGS), r#"{"theme":"dark","reasoning":"low"}"#).expect("write");
        let model = models::find(Provider::OpenAi, "gpt-5.6-luna");
        save_model(home, Provider::OpenAi, &model).expect("save");
        save_effort(home, None).expect("save");
        let settings = read(home, SETTINGS).expect("read");
        assert_eq!(settings["theme"], "dark");
        assert_eq!(settings["model"]["provider"], "openai");
        assert!(!settings.contains_key("reasoning"), "{settings:?}");
    }

    #[test]
    fn credentials_are_saved_readable_only_by_the_user() {
        let dir = tempfile::tempdir().expect("temp dir");
        let home = dir.path();
        let account = Some("acct".into());
        let tokens = Tokens { access: "a".into(), refresh: "r".into(), expires: 9, account };
        save_credential(home, Provider::OpenRouter, &Credential::Key("sk-1".into())).expect("key");
        save_credential(home, Provider::Codex, &Credential::Tokens(tokens.clone()))
            .expect("tokens");
        assert_eq!(read(home, AUTH).expect("read")["openrouter"], "sk-1");
        assert_eq!(saved_tokens(home, Provider::Codex), Some(tokens));
        assert_eq!(saved_tokens(home, Provider::Grok), None);
        let mode = fs::metadata(home.join(AUTH)).expect("metadata").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "auth.json mode {mode:o}");
        let left: Vec<_> = fs::read_dir(home)
            .expect("list")
            .flatten()
            .map(|entry| entry.file_name())
            .filter(|name| name.to_string_lossy().starts_with('.'))
            .collect();
        assert!(left.is_empty(), "temporary files left behind: {left:?}");
    }
}
