//! Playing videos and music inside the app.
//!
//! The pieces both players share live here: the one My files opens a video or
//! a song in ([`open_media_player`]) and the Photos lightbox. A GTK
//! [`gtk4::Video`] brings play, pause, a seek bar and the volume; this adds
//! fullscreen, the keys every video player answers to, a repeat switch and a
//! volume that carries over from one video to the next.

use crate::activation::{mounted_path, present_within};
use crate::*;

/// How far `J` and `L` (or Shift+←/→) jump.
const SEEK_LONG: i64 = 10;

/// How far ← and → jump in the My files player, where they don't move to
/// another photo.
const SEEK_SHORT: i64 = 5;

/// How much ↑ and ↓ change the volume in the My files player.
const VOLUME_STEP: f64 = 0.1;

thread_local! {
    /// The volume and mute the user last set, applied to the next stream so
    /// muting one video doesn't leave the next one blaring. Kept for the run
    /// of the app only.
    static LEVEL: Cell<Option<(f64, bool)>> = const { Cell::new(None) };
}

/// Where a jump of `delta_secs` from `now` lands, in microseconds, as a
/// [`gtk4::MediaStream`] counts. Kept inside the stream; a `duration` of 0
/// means the stream doesn't know its length yet, which bounds only the start.
pub(crate) fn seek_target(now: i64, duration: i64, delta_secs: i64) -> i64 {
    let target = now
        .saturating_add(delta_secs.saturating_mul(1_000_000))
        .max(0);
    if duration > 0 {
        target.min(duration)
    } else {
        target
    }
}

/// Jump the stream `delta_secs` forward, or back when negative.
pub(crate) fn seek_by(video: &gtk4::Video, delta_secs: i64) {
    if let Some(stream) = video.media_stream()
        && stream.is_seekable()
    {
        stream.seek(seek_target(
            stream.timestamp(),
            stream.duration(),
            delta_secs,
        ));
    }
}

/// Play a paused stream, pause a playing one.
pub(crate) fn toggle_playing(video: &gtk4::Video) {
    if let Some(stream) = video.media_stream() {
        stream.set_playing(!stream.is_playing());
    }
}

fn toggle_muted(video: &gtk4::Video) {
    if let Some(stream) = video.media_stream() {
        stream.set_muted(!stream.is_muted());
    }
}

fn change_volume(video: &gtk4::Video, delta: f64) {
    if let Some(stream) = video.media_stream() {
        stream.set_muted(false);
        stream.set_volume((stream.volume() + delta).clamp(0.0, 1.0));
    }
}

/// Enter fullscreen, or leave it.
pub(crate) fn toggle_fullscreen(window: &gtk4::Window) {
    if window.is_fullscreen() {
        window.unfullscreen();
    } else {
        window.fullscreen();
    }
}

/// The keys both players answer to the same way: `K` plays and pauses, `J` and
/// `L` (or Shift+← and Shift+→) jump ten seconds, `M` mutes. True when `key`
/// was one of them.
pub(crate) fn player_key(video: &gtk4::Video, key: &str, state: gtk4::gdk::ModifierType) -> bool {
    let shift = state.contains(gtk4::gdk::ModifierType::SHIFT_MASK);
    let plain = !state
        .intersects(gtk4::gdk::ModifierType::CONTROL_MASK | gtk4::gdk::ModifierType::ALT_MASK);
    if !plain {
        return false;
    }
    match key {
        "k" | "K" => toggle_playing(video),
        "j" | "J" => seek_by(video, -SEEK_LONG),
        "l" | "L" => seek_by(video, SEEK_LONG),
        "Left" if shift => seek_by(video, -SEEK_LONG),
        "Right" if shift => seek_by(video, SEEK_LONG),
        "m" | "M" => toggle_muted(video),
        _ => return false,
    }
    true
}

/// Make `video` carry the volume over between streams and toggle `window`'s
/// fullscreen on a double-click.
pub(crate) fn wire_player(video: &gtk4::Video, window: &gtk4::Window) {
    video.connect_media_stream_notify(|video| {
        let Some(stream) = video.media_stream() else {
            return;
        };
        if let Some((volume, muted)) = LEVEL.get() {
            stream.set_volume(volume);
            stream.set_muted(muted);
        }
        let remember = |stream: &gtk4::MediaStream| {
            LEVEL.set(Some((stream.volume(), stream.is_muted())));
        };
        stream.connect_volume_notify(remember);
        stream.connect_muted_notify(remember);
    });

    // The controls' own buttons and seek bar claim their clicks, so this only
    // sees a double-click on the picture.
    let click = gtk4::GestureClick::new();
    let window = window.downgrade();
    click.connect_pressed(move |_, n_press, _, _| {
        if n_press == 2
            && let Some(window) = window.upgrade()
        {
            toggle_fullscreen(&window);
        }
    });
    video.add_controller(click);
}

/// A button that enters and leaves `window`'s fullscreen, showing which one
/// it will do.
pub(crate) fn fullscreen_button(window: &gtk4::Window) -> gtk4::Button {
    let button = gtk4::Button::builder().valign(gtk4::Align::Center).build();
    let paint = |button: &gtk4::Button, fullscreen: bool| {
        if fullscreen {
            button.set_icon_name("view-restore-symbolic");
            button.set_tooltip_text(Some(&gettext("Leave Fullscreen (F)")));
        } else {
            button.set_icon_name("view-fullscreen-symbolic");
            button.set_tooltip_text(Some(&gettext("Fullscreen (F)")));
        }
    };
    paint(&button, window.is_fullscreen());
    let weak = button.downgrade();
    window.connect_fullscreened_notify(move |window| {
        if let Some(button) = weak.upgrade() {
            paint(&button, window.is_fullscreen());
        }
    });
    let target = window.downgrade();
    button.connect_clicked(move |_| {
        if let Some(window) = target.upgrade() {
            toggle_fullscreen(&window);
        }
    });
    button
}

/// Play a video or a song from My files in a window of its own, streamed from
/// the mount: the player reads the blocks it needs as it plays and seeks, so a
/// long video starts in seconds instead of after a full download. A file the
/// mount doesn't show, or the service is down, opens in the default
/// application instead, after a download.
pub(crate) fn open_media_player(ui: &Rc<Ui>, entry: &DirEntry) {
    let rel = entry_rel(ui, entry);
    let mountpoint = ui.dirs.resolved_mountpoint(&ui.dirs.load_config());
    let (ui, entry) = (ui.clone(), entry.clone());
    glib::spawn_future_local(async move {
        match present_within(mounted_path(&mountpoint, &rel)).await {
            Some(path) => build_media_player(&ui, &entry, &path),
            None => download_and_open(&ui, &entry),
        }
    });
}

fn build_media_player(ui: &Rc<Ui>, entry: &DirEntry, path: &Path) {
    let window = adw::Window::builder()
        .title(&entry.name)
        .default_width(960)
        .default_height(600)
        .build();
    if let Some(parent) = ui_window(ui) {
        window.set_transient_for(Some(&parent));
    }
    window.add_css_class("media-player-window");
    let gtk_window: gtk4::Window = window.clone().upcast();

    let video = gtk4::Video::builder()
        .autoplay(true)
        .hexpand(true)
        .vexpand(true)
        .build();
    wire_player(&video, &gtk_window);

    // A song has no picture, which would leave a black window above the
    // controls: a note stands in for it.
    let note = gtk4::Image::builder()
        .icon_name("audio-x-generic-symbolic")
        .pixel_size(128)
        .halign(gtk4::Align::Center)
        .valign(gtk4::Align::Center)
        .can_target(false)
        .visible(false)
        .build();
    note.add_css_class("media-player-note");
    let overlay = gtk4::Overlay::new();
    overlay.set_child(Some(&video));
    overlay.add_overlay(&note);

    let failed = adw::StatusPage::builder()
        .icon_name("dialog-warning-symbolic")
        .title(gettext("Couldn't play this file"))
        .build();
    let open_with = gtk4::Button::builder()
        .label(gettext("Open With…"))
        .halign(gtk4::Align::Center)
        .build();
    open_with.add_css_class("pill");
    open_with.add_css_class("suggested-action");
    failed.set_child(Some(&open_with));

    let stack = gtk4::Stack::new();
    stack.add_named(&overlay, Some("player"));
    stack.add_named(&failed, Some("failed"));

    let header = adw::HeaderBar::new();
    header.pack_end(&fullscreen_button(&gtk_window));
    let mut menu = ActionMenu::new();
    let (ui_c, entry_c) = (ui.clone(), entry.clone());
    menu.item(&gettext("Open With…"), move || {
        open_entry_with(&ui_c, &entry_c)
    });
    let video_c = video.clone();
    // Translators: menu item of the video player that plays the file again
    // and again.
    menu.toggle(&gettext("Repeat"), false, move |on| video_c.set_loop(on));
    header.pack_end(&menu.button());

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&stack));
    // Fullscreen is the video alone, its own controls still at hand.
    window
        .bind_property("fullscreened", &toolbar, "reveal-top-bars")
        .invert_boolean()
        .sync_create()
        .build();
    window.set_content(Some(&toolbar));

    let (ui_c, entry_c) = (ui.clone(), entry.clone());
    open_with.connect_clicked(move |_| open_entry_with(&ui_c, &entry_c));

    let (stack_c, failed_c, note_c) = (stack.clone(), failed.clone(), note.clone());
    video.connect_media_stream_notify(move |video| {
        let Some(stream) = video.media_stream() else {
            return;
        };
        let (stack, failed) = (stack_c.clone(), failed_c.clone());
        stream.connect_error_notify(move |stream| {
            if let Some(error) = stream.error() {
                tracing::warn!("in-app player failed: {error}");
                failed.set_description(Some(&error.to_string()));
                stack.set_visible_child_name("failed");
            }
        });
        let note = note_c.clone();
        stream.connect_prepared_notify(move |stream| {
            note.set_visible(stream.is_prepared() && stream.has_audio() && !stream.has_video());
        });
    });
    video.set_filename(Some(path));

    let keys = gtk4::EventControllerKey::new();
    let (video_k, window_k) = (video.clone(), gtk_window.downgrade());
    keys.connect_key_pressed(move |_, key, _code, state| {
        let Some(window) = window_k.upgrade() else {
            return glib::Propagation::Proceed;
        };
        let Some(name) = key.name() else {
            return glib::Propagation::Proceed;
        };
        // The open menu keeps its arrows and Enter.
        let in_menu = gtk4::prelude::GtkWindowExt::focus(&window)
            .is_some_and(|focus| focus.ancestor(gtk4::Popover::static_type()).is_some());
        if in_menu {
            return glib::Propagation::Proceed;
        }
        if player_key(&video_k, &name, state) {
            return glib::Propagation::Stop;
        }
        let ctrl = state.contains(gtk4::gdk::ModifierType::CONTROL_MASK);
        match name.as_str() {
            "space" => toggle_playing(&video_k),
            "Left" => seek_by(&video_k, -SEEK_SHORT),
            "Right" => seek_by(&video_k, SEEK_SHORT),
            "Up" => change_volume(&video_k, VOLUME_STEP),
            "Down" => change_volume(&video_k, -VOLUME_STEP),
            "f" | "F11" => toggle_fullscreen(&window),
            // Escape leaves fullscreen before it closes anything.
            "Escape" if window.is_fullscreen() => window.unfullscreen(),
            "Escape" | "q" => window.close(),
            "w" if ctrl => window.close(),
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    });
    // Capture, so the arrows and Space reach the player rather than moving the
    // focus between its controls.
    keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
    window.add_controller(keys);

    // Stop the sound with the window, not when the window is finally freed.
    let video_c = video.clone();
    window.connect_close_request(move |_| {
        video_c.set_media_stream(gtk4::MediaStream::NONE);
        glib::Propagation::Proceed
    });
    window.present();
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: i64 = 1_000_000;

    #[test]
    fn a_jump_stays_inside_the_video() {
        assert_eq!(seek_target(30 * SEC, 60 * SEC, 10), 40 * SEC);
        assert_eq!(seek_target(30 * SEC, 60 * SEC, -10), 20 * SEC);
        assert_eq!(seek_target(5 * SEC, 60 * SEC, -10), 0);
        assert_eq!(seek_target(55 * SEC, 60 * SEC, 10), 60 * SEC);
    }

    #[test]
    fn a_jump_forward_in_a_stream_of_unknown_length_is_not_held_back() {
        assert_eq!(seek_target(30 * SEC, 0, 10), 40 * SEC);
    }
}
