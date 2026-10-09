use crate::*;

/// The "›" between two breadcrumb segments.
pub(crate) fn crumb_separator() -> gtk4::Label {
    let sep = gtk4::Label::new(Some("›"));
    sep.add_css_class("dim-label");
    sep
}

/// The last breadcrumb segment, the folder on screen: a heading rather than a
/// button. It carries a flat button's side padding (`.crumb-current`), so the
/// "›" before it sits as far from it as from the button before.
pub(crate) fn crumb_current(label: &str) -> gtk4::Widget {
    let l = gtk4::Label::builder()
        .label(label)
        .ellipsize(gtk4::pango::EllipsizeMode::Start)
        .build();
    l.add_css_class("heading");
    l.add_css_class("crumb-current");
    l.upcast()
}
