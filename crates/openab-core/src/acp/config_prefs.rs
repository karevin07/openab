//! Durable ACP config choices (model, mode) and the views Discord renders.
//!
//! An agent only reports its config options inside a live session, and a
//! choice made with `session/set_config_option` lives only as long as that
//! session. This module keeps the choices and the last option list the agent
//! reported, so a thread can pick a model before its session exists and keep
//! that model across a reset or restart.
//!
//! Precedence, lowest first: `[pool] default_config_options`, the bot-wide
//! default chosen from Discord, the thread's own choice.

use crate::acp::protocol::ConfigOption;
use std::collections::{BTreeMap, HashMap};

/// Does `option` belong to the UI `category`?
///
/// `/session agent` asks for the `agent` category, but OpenCode reports its
/// build/plan switch as category `mode`; both describe the same control.
pub fn category_matches(option: &ConfigOption, category: &str) -> bool {
    match option.category.as_deref() {
        Some(found) if found == category => true,
        Some("mode") => category == "agent",
        _ => false,
    }
}

pub fn find_category<'a>(options: &'a [ConfigOption], category: &str) -> Option<&'a ConfigOption> {
    options
        .iter()
        .find(|option| category_matches(option, category))
}

/// Display name of the current value in `category`, falling back to the raw
/// value when the agent did not name it.
pub fn current_label(options: &[ConfigOption], category: &str) -> Option<String> {
    let option = find_category(options, category)?;
    if option.current_value.is_empty() {
        return None;
    }
    Some(
        option
            .options
            .iter()
            .find(|value| value.value == option.current_value)
            .map(|value| value.name.clone())
            .unwrap_or_else(|| option.current_value.clone()),
    )
}

fn offers(option: &ConfigOption, value: &str) -> bool {
    option
        .options
        .iter()
        .any(|candidate| candidate.value == value)
}

#[derive(Debug, Default)]
pub(crate) struct ConfigPreferences {
    /// thread key → config id → chosen value.
    pub thread: HashMap<String, HashMap<String, String>>,
    /// config id → value, chosen from Discord.
    pub defaults: HashMap<String, String>,
    /// Option list from the most recent fresh `session/new`, before any choice
    /// was applied, so its current values are the agent's own defaults.
    pub catalog: Vec<ConfigOption>,
    /// Last options each live session reported.
    pub views: HashMap<String, Vec<ConfigOption>>,
}

impl ConfigPreferences {
    /// Values to apply to `thread`, in config-id order. Defaults are included
    /// only for a fresh session; a resumed session keeps whatever the agent
    /// restored unless the thread made its own choice.
    pub fn desired(
        &self,
        thread: &str,
        file_defaults: &HashMap<String, String>,
        include_defaults: bool,
    ) -> BTreeMap<String, String> {
        let mut desired = BTreeMap::new();
        if include_defaults {
            desired.extend(file_defaults.iter().map(|(k, v)| (k.clone(), v.clone())));
            desired.extend(self.defaults.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        if let Some(choices) = self.thread.get(thread) {
            desired.extend(choices.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        desired
    }

    /// Option list to show for `thread`, with each current value replaced by
    /// what the thread will actually run with: its live value, overridden by a
    /// choice not yet applied (the session was busy or not running). `live`
    /// says whether the thread has a running session; a view recorded for a
    /// session that has since gone away is ignored.
    pub fn view(
        &self,
        thread: &str,
        file_defaults: &HashMap<String, String>,
        live: bool,
    ) -> Vec<ConfigOption> {
        let (mut options, desired) = match self.views.get(thread).filter(|_| live) {
            Some(options) => (options.clone(), self.desired(thread, file_defaults, false)),
            None => (self.base(), self.desired(thread, file_defaults, true)),
        };
        overlay(&mut options, &desired);
        options
    }

    /// Option list for choosing the bot-wide default.
    pub fn default_view(&self, file_defaults: &HashMap<String, String>) -> Vec<ConfigOption> {
        let mut options = self.base();
        let mut desired: BTreeMap<String, String> = file_defaults
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        desired.extend(self.defaults.iter().map(|(k, v)| (k.clone(), v.clone())));
        overlay(&mut options, &desired);
        options
    }

    /// The fresh-session catalog, or any live session's list before the first
    /// fresh session has been seen.
    fn base(&self) -> Vec<ConfigOption> {
        if !self.catalog.is_empty() {
            return self.catalog.clone();
        }
        self.views
            .values()
            .find(|options| !options.is_empty())
            .cloned()
            .unwrap_or_default()
    }
}

fn overlay(options: &mut [ConfigOption], desired: &BTreeMap<String, String>) {
    for option in options {
        if let Some(value) = desired.get(&option.id) {
            if offers(option, value) {
                option.current_value = value.clone();
            }
        }
    }
}

/// Is `value` a valid choice for config `config_id` in `options`?
pub fn validate_choice(
    options: &[ConfigOption],
    config_id: &str,
    value: &str,
) -> Result<(), String> {
    let Some(option) = options.iter().find(|option| option.id == config_id) else {
        return Err(format!("目前沒有可用的 `{config_id}` 選項"));
    };
    if !offers(option, value) {
        return Err(format!("`{value}` 不是可用的 {} 選項", option.name));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::protocol::ConfigOptionValue;

    fn model_option(current: &str) -> ConfigOption {
        ConfigOption {
            id: "model".into(),
            name: "Model".into(),
            description: None,
            category: Some("model".into()),
            option_type: "select".into(),
            current_value: current.into(),
            options: ["a", "b", "c"]
                .into_iter()
                .map(|value| ConfigOptionValue {
                    value: value.into(),
                    name: value.to_uppercase(),
                    description: None,
                })
                .collect(),
        }
    }

    #[test]
    fn thread_choice_beats_discord_default_beats_file_default() {
        let mut prefs = ConfigPreferences::default();
        let file = HashMap::from([("model".to_string(), "a".to_string())]);
        assert_eq!(prefs.desired("t", &file, true)["model"], "a");
        prefs.defaults.insert("model".into(), "b".into());
        assert_eq!(prefs.desired("t", &file, true)["model"], "b");
        prefs
            .thread
            .entry("t".into())
            .or_default()
            .insert("model".into(), "c".into());
        assert_eq!(prefs.desired("t", &file, true)["model"], "c");
        // A resumed session only gets the thread's own choice.
        assert!(prefs.desired("other", &file, false).is_empty());
    }

    #[test]
    fn view_without_session_shows_what_a_new_session_would_use() {
        let mut prefs = ConfigPreferences {
            catalog: vec![model_option("a")],
            ..Default::default()
        };
        prefs.defaults.insert("model".into(), "b".into());
        let view = prefs.view("t", &HashMap::new(), true);
        assert_eq!(current_label(&view, "model").as_deref(), Some("B"));
    }

    #[test]
    fn view_of_live_session_ignores_defaults_but_shows_pending_choice() {
        let mut prefs = ConfigPreferences::default();
        prefs.views.insert("t".into(), vec![model_option("a")]);
        prefs.defaults.insert("model".into(), "b".into());
        assert_eq!(
            current_label(&prefs.view("t", &HashMap::new(), true), "model").as_deref(),
            Some("A")
        );
        prefs
            .thread
            .entry("t".into())
            .or_default()
            .insert("model".into(), "c".into());
        assert_eq!(
            current_label(&prefs.view("t", &HashMap::new(), true), "model").as_deref(),
            Some("C")
        );
    }

    #[test]
    fn view_of_ended_session_falls_back_to_defaults() {
        let mut prefs = ConfigPreferences {
            catalog: vec![model_option("a")],
            ..Default::default()
        };
        prefs.views.insert("t".into(), vec![model_option("c")]);
        prefs.defaults.insert("model".into(), "b".into());
        assert_eq!(
            current_label(&prefs.view("t", &HashMap::new(), false), "model").as_deref(),
            Some("B")
        );
    }

    #[test]
    fn unknown_value_is_never_shown_or_accepted() {
        let mut prefs = ConfigPreferences {
            catalog: vec![model_option("a")],
            ..Default::default()
        };
        prefs.defaults.insert("model".into(), "gone".into());
        assert_eq!(
            current_label(&prefs.view("t", &HashMap::new(), true), "model").as_deref(),
            Some("A")
        );
        assert!(validate_choice(&prefs.catalog, "model", "gone").is_err());
        assert!(validate_choice(&prefs.catalog, "model", "b").is_ok());
        assert!(validate_choice(&prefs.catalog, "mode", "plan").is_err());
    }

    #[test]
    fn agent_category_matches_opencode_mode() {
        let mut mode = model_option("a");
        mode.id = "mode".into();
        mode.category = Some("mode".into());
        assert!(find_category(std::slice::from_ref(&mode), "agent").is_some());
        assert!(find_category(std::slice::from_ref(&mode), "model").is_none());
    }
}
