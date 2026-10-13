//! Background title work never owns a provider conversation or touches GPUI.

#[cfg(not(test))]
use fx_agent::title::TitleRequest;
use uuid::Uuid;

#[derive(Debug)]
pub(crate) struct TitleJob {
    pub generation: Uuid,
    pub provider: String,
}

#[cfg(not(test))]
pub(crate) fn spawn(
    request: TitleRequest,
    generation: Uuid,
    on_update: impl Fn(crate::session::SessionUpdate) + Send + 'static,
) -> Result<(), String> {
    std::thread::Builder::new()
        .name("flowx-chat-name".into())
        .spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| error.to_string())
                .and_then(|runtime| {
                    runtime.block_on(fx_agent::title::generate(
                        &request,
                        &fx_core::services::runtime::RuntimeDiscovery::default(),
                    ))
                });
            on_update(crate::session::SessionUpdate::AutoTitle { generation, result });
        })
        .map(|_| ())
        .map_err(|error| error.to_string())
}

pub(crate) fn fallback_title(prompt: &str) -> Option<String> {
    let normalized = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return None;
    }
    let mut title: String = normalized.chars().take(64).collect();
    if normalized.chars().count() > 64 {
        title.push('…');
    }
    Some(title)
}
