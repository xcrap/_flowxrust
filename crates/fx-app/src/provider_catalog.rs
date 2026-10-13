//! Nonblocking provider catalogs. Discovery never starts a conversation, and a
//! failed refresh preserves the last usable choices instead of emptying menus.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use fx_agent::claude::catalog::ModelCatalog;
use fx_agent::{Model, codex::CodexProvider};
use fx_core::services::runtime::RuntimeDiscovery;

const REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(20);

struct Catalog {
    models: Vec<Model>,
    aliases: HashMap<String, String>,
    capabilities_known: bool,
    running: bool,
    last_attempt: Option<Instant>,
    warning: Option<String>,
}

impl Catalog {
    fn new(provider: &str) -> Self {
        Self {
            models: fallback_models(provider),
            aliases: HashMap::new(),
            // Codex has a concrete fallback catalog. Claude's sole fallback
            // is an alias with unknown capabilities until initialization.
            capabilities_known: provider == "codex",
            running: false,
            last_attempt: None,
            warning: None,
        }
    }

    fn finish(&mut self, result: Result<ModelCatalog, String>) {
        self.running = false;
        match result {
            Ok(catalog) if !catalog.models.is_empty() => {
                self.models = catalog.models;
                self.aliases = catalog.aliases;
                self.capabilities_known = true;
                self.warning = None;
            }
            Ok(_) => self.warning = Some("The provider returned no model choices.".into()),
            Err(reason) => self.warning = Some(reason),
        }
    }

    fn model(&self, id: &str) -> Option<Model> {
        let lookup = self.aliases.get(id).map(String::as_str).unwrap_or(id);
        self.models.iter().find(|model| model.id == lookup).cloned()
    }

    fn known_model(&self, id: &str) -> Option<Model> {
        self.capabilities_known.then(|| self.model(id)).flatten()
    }

    fn begin(&mut self, force: bool) -> bool {
        if self.running
            || !force
                && self
                    .last_attempt
                    .is_some_and(|last| last.elapsed() < REFRESH_INTERVAL)
        {
            return false;
        }
        self.running = true;
        self.last_attempt = Some(Instant::now());
        true
    }
}

fn catalogs() -> &'static Mutex<HashMap<String, Catalog>> {
    static CATALOGS: OnceLock<Mutex<HashMap<String, Catalog>>> = OnceLock::new();
    CATALOGS.get_or_init(Default::default)
}

pub fn models(provider: &str) -> Vec<Model> {
    let mut catalogs = catalogs().lock().unwrap_or_else(|error| error.into_inner());
    catalogs
        .entry(provider.into())
        .or_insert_with(|| Catalog::new(provider))
        .models
        .clone()
}

/// Resolve both a native menu value and a saved concrete provider model ID.
pub fn model(provider: &str, id: &str) -> Option<Model> {
    let mut catalogs = catalogs().lock().unwrap_or_else(|error| error.into_inner());
    let catalog = catalogs
        .entry(provider.into())
        .or_insert_with(|| Catalog::new(provider));
    catalog.model(id)
}

/// Return capability metadata only when it is known. A placeholder model may
/// still be displayed while discovery runs, but must not erase saved choices.
pub fn known_model(provider: &str, id: &str) -> Option<Model> {
    let mut catalogs = catalogs().lock().unwrap_or_else(|error| error.into_inner());
    catalogs
        .entry(provider.into())
        .or_insert_with(|| Catalog::new(provider))
        .known_model(id)
}

/// Resolve an effort against the actual selected model. An empty advertised
/// list means the provider takes no effort parameter (for example Haiku).
pub fn effective_effort(
    provider: &str,
    model_id: &str,
    configured: Option<&str>,
) -> Option<String> {
    let model = known_model(provider, model_id);
    resolve_effort(model.as_ref(), configured)
}

fn resolve_effort(model: Option<&Model>, configured: Option<&str>) -> Option<String> {
    let Some(model) = model else {
        // Custom model IDs have no known capabilities; keep an explicit choice
        // but let the provider choose its default when none was requested.
        return configured.map(str::to_owned);
    };
    configured
        .filter(|effort| model.supported_efforts.iter().any(|known| known == effort))
        .or_else(|| {
            model
                .default_effort
                .as_deref()
                .filter(|effort| model.supported_efforts.iter().any(|known| known == effort))
        })
        .or_else(|| {
            model
                .supported_efforts
                .iter()
                .find(|effort| *effort == fx_core::DEFAULT_EFFORT)
                .map(String::as_str)
        })
        .or_else(|| model.supported_efforts.first().map(String::as_str))
        .map(str::to_owned)
}

pub fn warning(provider: &str) -> Option<String> {
    catalogs()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(provider)
        .and_then(|catalog| catalog.warning.clone())
}

pub fn is_refreshing() -> bool {
    catalogs()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .values()
        .any(|catalog| catalog.running)
}

/// Refresh at most once per interval and once concurrently per provider. Safe
/// to call when opening a picker; it returns immediately on the UI thread.
pub fn refresh(provider: &str) -> bool {
    start_refresh(provider, false)
}

/// An explicit user refresh bypasses the cooldown, but never duplicates a worker.
pub fn refresh_now(provider: &str) -> bool {
    start_refresh(provider, true)
}

fn start_refresh(provider: &str, force: bool) -> bool {
    if !matches!(provider, "codex" | "claude") {
        return false;
    }
    {
        let mut catalogs = catalogs().lock().unwrap_or_else(|error| error.into_inner());
        let catalog = catalogs
            .entry(provider.into())
            .or_insert_with(|| Catalog::new(provider));
        if !catalog.begin(force) {
            return false;
        }
    }
    let provider_id = provider.to_owned();
    let worker = std::thread::Builder::new()
        .name(format!("flowx-{provider}-models"))
        .spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| error.to_string())
                .and_then(|runtime| {
                    runtime.block_on(async {
                        tokio::time::timeout(DISCOVERY_TIMEOUT, discover(&provider_id))
                            .await
                            .map_err(|_| "Provider model discovery timed out.".to_owned())?
                    })
                });
            if let Some(catalog) = catalogs()
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get_mut(&provider_id)
            {
                catalog.finish(result);
            }
        });
    if let Err(error) = worker {
        if let Some(catalog) = catalogs()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get_mut(provider)
        {
            catalog.finish(Err(error.to_string()));
        }
        return false;
    }
    true
}

async fn discover(provider: &str) -> Result<ModelCatalog, String> {
    let discovery = RuntimeDiscovery::default();
    if provider == "codex" {
        return CodexProvider::new(discovery)
            .discover_models()
            .await
            .map(|models| ModelCatalog {
                models,
                aliases: HashMap::new(),
            })
            .map_err(|error| error.to_string());
    }
    fx_agent::claude::catalog::discover_models(&discovery).await
}

pub fn fallback_models(provider: &str) -> Vec<Model> {
    if provider == "codex" {
        return fx_agent::codex::fallback_models();
    }
    if provider != "claude" {
        return Vec::new();
    }
    // `default` is a stable CLI choice, not a claim about which model or effort
    // the account currently has. Real choices come only from initialization.
    vec![Model {
        id: "default".into(),
        display_name: "Provider default".into(),
        context_window: 0,
        max_context_window: 0,
        supported_efforts: Vec::new(),
        default_effort: None,
        supports_vision: false,
        is_default: true,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_claude_fallback_preserves_reasoning_until_capabilities_are_discovered() {
        let mut catalog = Catalog::new("claude");
        assert!(catalog.model("default").is_some());
        assert!(catalog.known_model("default").is_none());
        assert_eq!(
            resolve_effort(catalog.known_model("default").as_ref(), Some("high")).as_deref(),
            Some("high")
        );
        catalog.finish(Err("discovery unavailable".into()));
        assert_eq!(
            resolve_effort(catalog.known_model("default").as_ref(), Some("high")).as_deref(),
            Some("high")
        );

        // A discovered empty list genuinely disallows a reasoning parameter.
        catalog.finish(Ok(ModelCatalog {
            models: fallback_models("claude"),
            aliases: HashMap::new(),
        }));
        assert!(catalog.known_model("default").is_some());
        assert_eq!(
            resolve_effort(catalog.known_model("default").as_ref(), Some("high")),
            None
        );
        catalog.finish(Err("later refresh failed".into()));
        assert_eq!(
            resolve_effort(catalog.known_model("default").as_ref(), Some("high")),
            None
        );
    }

    #[test]
    fn a_failed_or_empty_refresh_preserves_the_last_available_choices() {
        let mut catalog = Catalog::new("codex");
        let mut discovered = catalog.models[0].clone();
        discovered.id = "account-specific-model".into();
        catalog.finish(Ok(ModelCatalog {
            models: vec![discovered.clone()],
            aliases: HashMap::from([("alias".into(), discovered.id.clone())]),
        }));
        catalog.finish(Err("offline".into()));
        assert_eq!(catalog.models, vec![discovered.clone()]);
        assert_eq!(catalog.warning.as_deref(), Some("offline"));
        assert!(!catalog.running);
        catalog.finish(Ok(ModelCatalog::default()));
        assert_eq!(catalog.models, vec![discovered]);
    }

    #[test]
    fn efforts_follow_model_capabilities_including_models_without_reasoning_control() {
        let mut model = fallback_models("claude").remove(0);
        assert_eq!(resolve_effort(Some(&model), Some("high")), None);
        model.supported_efforts = vec!["low".into(), "high".into(), "max".into()];
        model.default_effort = Some("high".into());
        assert_eq!(resolve_effort(Some(&model), None).as_deref(), Some("high"));
        assert_eq!(
            resolve_effort(Some(&model), Some("xhigh")).as_deref(),
            Some("high")
        );
        assert_eq!(
            resolve_effort(Some(&model), Some("low")).as_deref(),
            Some("low")
        );
        assert_eq!(resolve_effort(None, None), None);
    }

    #[test]
    fn a_successful_refresh_replaces_old_choices_and_aliases() {
        let mut catalog = Catalog::new("claude");
        catalog
            .aliases
            .insert("old-resolved-model".into(), "old".into());
        let mut model = catalog.models[0].clone();
        model.id = "new-native-value".into();
        catalog.finish(Ok(ModelCatalog {
            models: vec![model.clone()],
            aliases: HashMap::from([("new-resolved-model".into(), model.id.clone())]),
        }));
        assert_eq!(catalog.models, [model]);
        assert!(!catalog.aliases.contains_key("old-resolved-model"));
        assert_eq!(catalog.aliases["new-resolved-model"], "new-native-value");
        assert!(catalog.warning.is_none());
    }

    #[test]
    fn explicit_refresh_bypasses_cooldown_without_starting_duplicate_workers() {
        let mut catalog = Catalog::new("claude");
        assert!(catalog.begin(false));
        assert!(!catalog.begin(true));
        catalog.finish(Err("offline".into()));
        assert!(!catalog.begin(false));
        assert!(catalog.begin(true));
        assert!(!catalog.begin(true));
    }
}
