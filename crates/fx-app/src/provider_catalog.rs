//! Nonblocking provider catalogs. Discovery never starts a conversation, and a
//! failed refresh preserves the last usable choices instead of emptying menus.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use fx_agent::{Model, codex::CodexProvider};
use fx_core::services::runtime::{RuntimeDiscovery, claude_spec};

const REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(20);

struct Catalog {
    models: Vec<Model>,
    running: bool,
    last_attempt: Option<Instant>,
    warning: Option<String>,
}

impl Catalog {
    fn new(provider: &str) -> Self {
        Self {
            models: fallback_models(provider),
            running: false,
            last_attempt: None,
            warning: None,
        }
    }

    fn finish(&mut self, result: Result<Vec<Model>, String>) {
        self.running = false;
        match result {
            Ok(models) if !models.is_empty() => {
                self.models = models;
                self.warning = None;
            }
            Ok(_) => self.warning = Some("The provider returned no model choices.".into()),
            Err(reason) => self.warning = Some(reason),
        }
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

/// Resolve an effort against the actual selected model. An empty advertised
/// list means the provider takes no effort parameter (for example Haiku).
pub fn effective_effort(
    provider: &str,
    model_id: &str,
    configured: Option<&str>,
) -> Option<String> {
    let models = models(provider);
    resolve_effort(models.iter().find(|model| model.id == model_id), configured)
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
    if !matches!(provider, "codex" | "claude") {
        return false;
    }
    {
        let mut catalogs = catalogs().lock().unwrap_or_else(|error| error.into_inner());
        let catalog = catalogs
            .entry(provider.into())
            .or_insert_with(|| Catalog::new(provider));
        if catalog.running
            || catalog
                .last_attempt
                .is_some_and(|last| last.elapsed() < REFRESH_INTERVAL)
        {
            return false;
        }
        catalog.running = true;
        catalog.last_attempt = Some(Instant::now());
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

async fn discover(provider: &str) -> Result<Vec<Model>, String> {
    let discovery = RuntimeDiscovery::default();
    if provider == "codex" {
        return CodexProvider::new(discovery)
            .discover_models()
            .await
            .map_err(|error| error.to_string());
    }
    // Claude's help is a capability probe, not a model-list endpoint. Preserve
    // the complete curated Swift catalog, including models absent from help's
    // examples; the user's explicit/custom model remains a UI-level choice.
    let output = discovery
        .run(
            &claude_spec(),
            &["--help".into()],
            None,
            Duration::from_secs(10),
        )
        .await
        .map_err(|error| error.to_string())?;
    if !output.succeeded() || !output.stdout.contains("--model") {
        return Err(
            "Claude Code did not advertise model selection; using the saved catalog.".into(),
        );
    }
    Ok(fallback_models("claude"))
}

pub fn fallback_models(provider: &str) -> Vec<Model> {
    if provider == "codex" {
        return fx_agent::codex::fallback_models();
    }
    if provider != "claude" {
        return Vec::new();
    }
    // Kept in step with ClaudeCodeProvider.fallbackModels in the Swift app.
    [
        ("default", "Provider default", 0, true),
        ("claude-fable-5", "Claude Fable 5", 1_000_000, false),
        ("claude-opus-4-8", "Claude Opus 4.8", 1_000_000, false),
        ("claude-sonnet-5", "Claude Sonnet 5", 200_000, false),
        ("claude-haiku-4-5", "Claude Haiku 4.5", 200_000, false),
    ]
    .into_iter()
    .map(|(id, name, context, is_default)| {
        let has_effort = id != "claude-haiku-4-5";
        Model {
            id: id.into(),
            display_name: name.into(),
            context_window: context,
            max_context_window: context,
            supported_efforts: if has_effort {
                ["low", "medium", "high", "xhigh", "max"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect()
            } else {
                Vec::new()
            },
            default_effort: has_effort.then(|| "high".into()),
            supports_vision: true,
            is_default,
        }
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_or_empty_refresh_preserves_the_last_available_choices() {
        let mut catalog = Catalog::new("codex");
        let mut discovered = catalog.models[0].clone();
        discovered.id = "account-specific-model".into();
        catalog.finish(Ok(vec![discovered.clone()]));
        catalog.finish(Err("offline".into()));
        assert_eq!(catalog.models, vec![discovered.clone()]);
        assert_eq!(catalog.warning.as_deref(), Some("offline"));
        assert!(!catalog.running);
        catalog.finish(Ok(Vec::new()));
        assert_eq!(catalog.models, vec![discovered]);
    }

    #[test]
    fn efforts_follow_model_capabilities_including_models_without_reasoning_control() {
        let claude = fallback_models("claude");
        let haiku = claude.iter().find(|model| model.id == "claude-haiku-4-5");
        assert_eq!(resolve_effort(haiku, Some("high")), None);
        let opus = claude.iter().find(|model| model.id == "claude-opus-4-8");
        assert_eq!(resolve_effort(opus, None).as_deref(), Some("high"));
        assert_eq!(
            resolve_effort(opus, Some("unsupported")).as_deref(),
            Some("high")
        );
        assert_eq!(resolve_effort(opus, Some("low")).as_deref(), Some("low"));
        assert_eq!(resolve_effort(None, None), None);
    }
}
