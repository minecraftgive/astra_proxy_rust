//! Window ownership is presentation state; shared operations keep their existing model.
use super::*;
use astra_client::{
    jobs::Origin,
    operations::{ApplyContext, JobResult},
    state::Client,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Overlay {
    Editor,
    Diagnostics,
    Explanation,
    Bindings,
    Prompt,
    Recovery,
    Installation,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WindowEffect {
    Editor,
    Diagnostics,
    Rules,
    Profiles,
    Logs,
}
#[derive(Default)]
pub(super) struct Routing {
    owners: BTreeMap<Overlay, u64>,
    snapshot: BTreeMap<Overlay, String>,
    effects: BTreeMap<u64, Vec<WindowEffect>>,
    contexts: BTreeMap<u64, (u64, Origin)>,
    jobs: BTreeMap<u64, (u64, u64)>,
    editor_request: Option<u64>,
}
fn snapshot(client: &Client) -> BTreeMap<Overlay, String> {
    let mut result = BTreeMap::new();
    if let Some(editor) = &client.editor {
        result.insert(Overlay::Editor, editor.name.clone());
    }
    if let Some(view) = &client.diagnostics {
        result.insert(Overlay::Diagnostics, view.name.clone());
    }
    if client.explanation.is_some() {
        result.insert(Overlay::Explanation, String::new());
    }
    if let Some(picker) = &client.binding_picker {
        result.insert(Overlay::Bindings, picker.name.clone());
    }
    if let Some(prompt) = &client.prompt {
        result.insert(
            Overlay::Prompt,
            serde_json::to_string(prompt).unwrap_or_default(),
        );
    }
    if let Some(recovery) = &client.recovery {
        result.insert(
            Overlay::Recovery,
            serde_json::to_string(recovery).unwrap_or_default(),
        );
    }
    if client.installation.visible() {
        result.insert(Overlay::Installation, String::new());
    }
    result
}
impl AppModel {
    pub(super) fn forget_window_job(&mut self, id: u64) {
        self.routing.jobs.remove(&id);
    }
    pub(crate) fn editor_owned_by(&self, window: u64) -> bool {
        self.client.editor.is_some() && self.owns_overlay(Overlay::Editor, window)
    }
    pub(crate) fn owns_overlay(&self, overlay: Overlay, window: u64) -> bool {
        self.routing
            .owners
            .get(&overlay)
            .copied()
            .or_else(|| self.window_pages.keys().next().copied())
            == Some(window)
    }
    fn claim_overlay(&mut self, overlay: Overlay, window: u64) {
        self.routing.owners.insert(overlay, window);
        let effect = match overlay {
            Overlay::Editor => Some(WindowEffect::Editor),
            Overlay::Diagnostics => Some(WindowEffect::Diagnostics),
            Overlay::Explanation => Some(WindowEffect::Rules),
            Overlay::Bindings => Some(WindowEffect::Profiles),
            _ => None,
        };
        if let Some(effect) = effect {
            self.routing.effects.entry(window).or_default().push(effect);
        }
    }
    pub(crate) fn route_window_effects(&mut self, window: u64) {
        let current = snapshot(&self.client);
        let changed: Vec<_> = current
            .iter()
            .filter(|(key, value)| self.routing.snapshot.get(key) != Some(value))
            .map(|(key, _)| *key)
            .collect();
        self.routing
            .owners
            .retain(|key, _| current.contains_key(key));
        for overlay in changed {
            self.claim_overlay(overlay, window);
        }
        self.routing.snapshot = current;
    }
    pub(crate) fn take_window_effects(&mut self, window: u64) -> Vec<WindowEffect> {
        self.routing.effects.remove(&window).unwrap_or_default()
    }
    pub(crate) fn window_context(&mut self, window: u64, page: usize, invalidate: bool) {
        let mut origin = self.origin();
        origin.view_context.0 = page;
        let epoch = self
            .routing
            .contexts
            .get(&window)
            .map_or(0, |(epoch, _)| *epoch)
            + u64::from(invalidate);
        origin.overlay_epoch = epoch;
        self.routing.contexts.insert(window, (epoch, origin));
    }
    pub(crate) fn remove_window_routing(&mut self, window: u64) {
        if self.routing.owners.get(&Overlay::Editor) == Some(&window) {
            self.client.suspend_editor();
        }
        self.routing.contexts.remove(&window);
        self.routing.effects.remove(&window);
        self.routing.owners.retain(|_, owner| *owner != window);
    }
    pub(crate) fn start_window_prompt(
        &mut self,
        window: u64,
        page: usize,
        action: Action,
        prompt: Option<(astra_client::state::Prompt, String)>,
        cx: &mut Context<Self>,
    ) {
        let opens = matches!(
            action,
            Action::Read(_)
                | Action::Diagnostics(_)
                | Action::Logs(_)
                | Action::ExplainRoute { .. }
        );
        self.window_context(window, page, opens);
        // Shared capture needs the page of the initiating window.
        let previous_page = self.client.page;
        self.client.page = page;
        let before = self.next_job;
        self.start_prompt(action.clone(), prompt, cx);
        self.client.page = previous_page;
        if self.next_job != before
            && self
                .client
                .tasks
                .records
                .iter()
                .any(|task| task.id == self.next_job)
        {
            let epoch = self.routing.contexts[&window].0;
            self.routing.jobs.insert(self.next_job, (window, epoch));
            if matches!(action, Action::Read(_)) {
                self.routing.editor_request = Some(self.next_job);
            }
        }
    }
    pub(super) fn routed_apply_context(
        &mut self,
        id: u64,
        action: &Action,
        origin: &Origin,
    ) -> (ApplyContext, Option<u64>) {
        let Some((window, epoch)) = self.routing.jobs.remove(&id) else {
            return (
                origin.apply_context(action, &self.origin(), self.client.active_log.is_some()),
                None,
            );
        };
        let Some((current_epoch, current)) = self.routing.contexts.get(&window) else {
            return (
                ApplyContext {
                    same: false,
                    log_target_current: false,
                    background: true,
                },
                None,
            );
        };
        let mut context = origin.apply_context(action, current, self.client.active_log.is_some());
        if *current_epoch != epoch
            || (matches!(action, Action::Read(_)) && self.routing.editor_request != Some(id))
        {
            context.same = false;
            context.log_target_current = false;
        }
        // Frontend logs have their own visible view rather than an active instance log.
        if matches!(action, Action::Logs(name) if name == "@gui" || name == "@tui") {
            context.log_target_current = *current_epoch == epoch;
        }
        {
            let owner = context.same.then_some(window);
            (context, owner)
        }
    }
    pub(super) fn result_overlay(result: &JobResult) -> Option<Overlay> {
        match result {
            JobResult::Document { .. } => Some(Overlay::Editor),
            JobResult::Diagnostics { .. } => Some(Overlay::Diagnostics),
            JobResult::Explanation(_) => Some(Overlay::Explanation),
            _ => None,
        }
    }
    pub(super) fn finish_window_result(
        &mut self,
        owner: Option<u64>,
        overlay: Option<Overlay>,
        action: &Action,
    ) {
        if let Some(window) = owner {
            self.route_window_effects(window);
            if let Some(overlay) = overlay {
                self.claim_overlay(overlay, window);
            }
            if matches!(action, Action::Logs(name) if name != "@gui" && name != "@tui")
                && self.client.viewer.is_some()
            {
                self.routing
                    .effects
                    .entry(window)
                    .or_default()
                    .push(WindowEffect::Logs);
            }
        }
    }
}
