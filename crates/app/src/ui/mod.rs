pub mod agents;
pub mod approvals;
pub mod connectors;
pub mod credentials;
pub mod env_profiles;
pub mod file_tree;
pub mod notifications;
pub mod settings;
pub mod workspace;

pub fn render_message(catalog: &i18n::Catalog, message: &runtime::MessagePayload) -> String {
    let args: Vec<(&str, &str)> = message
        .args
        .iter()
        .map(|arg| (arg.key.as_str(), arg.value.as_str()))
        .collect();
    catalog.t(&message.message_id, &args)
}
