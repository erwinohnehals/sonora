use gpui::prelude::*;
use gpui::{App, Context, Entity, FocusHandle, Global, Render, Window, div};
use i18n::t;
use music::Playlist;
use state::{Addition, PinFolder, Shelf, Sonora};
use ui::{ActiveTheme as _, Button, Modal, Pin};
use ui::{Dismiss, FORM_CONTEXT, Input, Submit};

#[derive(Clone)]
pub(crate) enum Edit {
    Create {
        addition: Addition,
        shelf: Shelf,
    },
    Rename(Playlist),
    Delete(Playlist),
    Again {
        playlist: Playlist,
        track: String,
    },
    /// Names a new folder of pins, and files the pin in it when one is given.
    CreateFolder(Option<Pin>),
    RenameFolder(PinFolder),
}

/// The dialog that names, renames and deletes playlists, and names the folders of the pinned
/// list.
pub(crate) struct PlaylistEditor {
    edit: Option<Edit>,
    name: Entity<Input>,
    focus: FocusHandle,
    restore: Option<FocusHandle>,
}

struct Installed(Entity<PlaylistEditor>);

impl Global for Installed {}

impl PlaylistEditor {
    pub fn entity(cx: &mut App) -> Entity<Self> {
        if cx.try_global::<Installed>().is_none() {
            let editor = cx.new(|cx| Self {
                edit: None,
                name: cx.new(|cx| Input::new("playlist-name-placeholder", cx)),
                focus: cx.focus_handle(),
                restore: None,
            });
            cx.set_global(Installed(editor));
        }
        cx.global::<Installed>().0.clone()
    }

    pub fn open(edit: Edit, window: &mut Window, cx: &mut App) {
        let editor = Self::entity(cx);
        editor.update(cx, |this, cx| this.show(edit, window, cx));
    }

    fn show(&mut self, edit: Edit, window: &mut Window, cx: &mut Context<Self>) {
        let name = match &edit {
            Edit::Rename(playlist) => playlist.name.clone(),
            Edit::RenameFolder(folder) => folder.name.clone(),
            _ => String::new(),
        };
        let hint = match &edit {
            Edit::CreateFolder(_) | Edit::RenameFolder(_) => "pin-folder-name-placeholder",
            _ => "playlist-name-placeholder",
        };
        self.restore = window.focused(cx);
        self.name.update(cx, |input, cx| {
            input.set_text(name, cx);
            input.set_hint(hint, cx);
        });
        match plain(&edit) {
            true => window.focus(&self.focus, cx),
            false => self.name.update(cx, |input, cx| input.focus(window, cx)),
        }
        self.edit = Some(edit);
        cx.notify();
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.edit = None;
        if let Some(focus) = self.restore.take() {
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    fn apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.edit.take() else {
            return;
        };
        if let Some(focus) = self.restore.take() {
            window.focus(&focus, cx);
        }
        let name = self.name.read(cx).text().trim().to_owned();
        let library = Sonora::global(cx).library.clone();
        let pins = Sonora::global(cx).pins.clone();

        match edit {
            Edit::Create { addition, shelf } if !name.is_empty() => {
                library.update(cx, |library, cx| {
                    library.create_playlist(name, addition, shelf, cx);
                })
            }
            Edit::Rename(playlist) if !name.is_empty() && name != playlist.name => {
                library.update(cx, |library, cx| {
                    library.rename_playlist(playlist.id, name, cx);
                })
            }
            Edit::Delete(playlist) => library.update(cx, |library, cx| {
                library.delete_playlist(playlist.id, cx);
            }),
            Edit::Again { playlist, track } => library.update(cx, |library, cx| {
                library.add_to_playlist(playlist.id, track, cx);
            }),
            Edit::CreateFolder(pin) if !name.is_empty() => {
                pins.update(cx, |pins, cx| pins.create_folder(name, pin, cx))
            }
            Edit::RenameFolder(folder) if !name.is_empty() && name != folder.name => {
                pins.update(cx, |pins, cx| pins.rename_folder(folder.id, name, cx))
            }
            _ => {}
        }
        cx.notify();
    }
}

fn plain(edit: &Edit) -> bool {
    matches!(edit, Edit::Delete(_) | Edit::Again { .. })
}

impl Render for PlaylistEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(edit) = self.edit.clone() else {
            return div().into_any_element();
        };
        let theme = *cx.theme();
        let deleting = matches!(edit, Edit::Delete(_));
        let asking = plain(&edit);
        let title = match &edit {
            Edit::Create { .. } => t!("playlist-create-title"),
            Edit::Rename(_) => t!("playlist-rename-title"),
            Edit::Delete(_) => t!("playlist-delete-title"),
            Edit::Again { .. } => t!("playlist-again-title"),
            Edit::CreateFolder(_) => t!("pin-folder-create-title"),
            Edit::RenameFolder(_) => t!("pin-folder-rename-title"),
        };
        let detail = match &edit {
            Edit::Delete(playlist) => Some(t!("playlist-delete-confirm", name = &playlist.name)),
            Edit::Again { playlist, .. } => {
                Some(t!("playlist-again-confirm", name = &playlist.name))
            }
            _ => None,
        };

        div()
            .absolute()
            .inset_0()
            .key_context(FORM_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Dismiss, window, cx| {
                cx.stop_propagation();
                this.close(window, cx);
            }))
            .on_action(cx.listener(|this, _: &Submit, window, cx| {
                cx.stop_propagation();
                this.apply(window, cx);
            }))
            .child(
                Modal::new("playlist-editor", title)
                    .w(theme.metrics.cover * 2.8)
                    .when_some(detail, Modal::detail)
                    .when(!asking, |modal| modal.child(self.name.clone()))
                    .action(
                        Button::new("cancel-playlist-edit")
                            .ghost()
                            .label(t!("common-cancel"))
                            .on_click(cx.listener(|this, _, window, cx| this.close(window, cx))),
                    )
                    .action(
                        Button::new("apply-playlist-edit")
                            .when_else(
                                deleting,
                                |button| button.destructive(),
                                |button| button.primary(),
                            )
                            .label(match &edit {
                                Edit::Delete(_) => t!("common-delete"),
                                Edit::Again { .. } => t!("playlist-again-add"),
                                _ => t!("common-save"),
                            })
                            .on_click(cx.listener(|this, _, window, cx| this.apply(window, cx))),
                    )
                    .on_dismiss(cx.listener(|this, _, window, cx| this.close(window, cx))),
            )
            .into_any_element()
    }
}
