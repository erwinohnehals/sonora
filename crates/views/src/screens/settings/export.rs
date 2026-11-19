use gpui::prelude::*;
use gpui::{AnyElement, App, Context, PathPromptOptions, SharedString};
use i18n::t;
use state::{ExportNames, ExportState, Outcome, Toasts};
use ui::Button;

use super::SettingsView;
use super::rows::Row;

impl SettingsView {
    /// Whether the export row shows: only while a streaming account is signed in, since a guest
    /// session and the local files have no favorites or playlists of an account to export.
    pub(super) fn exportable(&self, cx: &App) -> bool {
        let session = self.session.read(cx);
        session.client().is_some() && !session.guest()
    }

    /// The row that writes the streaming library's favorites and playlists to a folder the
    /// listener picks. Its explanation follows the export while it runs and keeps the outcome.
    pub(super) fn export_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let export = self.export.read(cx);
        let running = export.running();
        let failed = matches!(export.state(), ExportState::Failed(_));
        let detail = export_detail(export.state());

        Row::new(
            t!("settings-export"),
            Button::new("export-library")
                .label(t!("settings-export-button"))
                .small()
                .outline()
                .disabled(running)
                .on_click(cx.listener(|this, _, _, cx| this.choose_export_folder(cx)))
                .into_any_element(),
        )
        .detail(detail)
        .alert(failed)
        .into_any_element()
    }

    /// Asks for the folder to export into, then starts the export there. The names of what it
    /// writes are resolved now, in the language the listener is reading.
    fn choose_export_folder(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        let provider = self.session.read(cx).provider_name().unwrap_or_default();
        let names = ExportNames {
            folder: t!("export-folder", provider = provider).to_string(),
            liked: t!("library-liked-songs").to_string(),
            playlists: t!("nav-playlists").to_string(),
        };
        let export = self.export.clone();
        cx.spawn(async move |_, cx| {
            let parent = match receiver.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Err(error)) => {
                    log::warn!("settings: cannot open the folder dialog: {error:#}");
                    cx.update(|cx| Toasts::show(Outcome::Failed, "toast-file-dialog-failed", cx));
                    None
                }
                _ => None,
            };
            let Some(parent) = parent else {
                return;
            };
            export.update(cx, |export, cx| export.start(parent, names, cx));
        })
        .detach();
    }
}

/// What the export row says under its label: what the export does while none has run, then how
/// far one has got, then what it saved or why it failed.
fn export_detail(state: &ExportState) -> SharedString {
    match state {
        ExportState::Idle => t!("settings-export-detail"),
        ExportState::Reading { total: 0, .. } => t!("settings-export-listing"),
        ExportState::Reading { done, total } => {
            t!("settings-export-reading", done = *done, total = *total)
        }
        ExportState::Writing => t!("settings-export-writing"),
        ExportState::Done {
            folder,
            tracks,
            skipped,
            ..
        } => {
            let name = folder
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let done = t!("settings-export-done", tracks = *tracks, folder = name);
            match skipped {
                0 => done,
                _ => format!(
                    "{done} · {}",
                    t!("settings-export-skipped", skipped = *skipped)
                )
                .into(),
            }
        }
        ExportState::Failed(reason) => t!("settings-export-failed", reason = reason.as_str()),
    }
}
