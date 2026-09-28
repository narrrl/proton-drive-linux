//! Reloading a page without flashing it.
//!
//! Pages used to swap their content for a "Loading…" status page on every
//! load, so every sidebar click and every folder change blinked. A [`Loader`]
//! keeps the last content on screen while the request runs and shows the
//! loading state only if the reply is slow. [`replace_items`] then swaps in the
//! new rows by changing only the ones that differ, so selection and scroll
//! position survive a refresh.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gtk4::gio;
use gtk4::glib;
use gtk4::glib::BoxedAnyObject;
use gtk4::glib::object::Cast;
use gtk4::prelude::*;

/// How long a load may run before the page gives up its old content for a
/// loading state. Most replies arrive well within this, so the page updates in
/// place.
const LOADING_GRACE: Duration = Duration::from_millis(300);

/// One page's loads. Each [`Loader::refresh`] or [`Loader::replace`]
/// supersedes the one before it.
pub(crate) struct Loader {
    content: gtk4::Widget,
    generation: Cell<u64>,
    placeholder: RefCell<Option<glib::SourceId>>,
}

impl Loader {
    /// A loader for the page whose content is `content`.
    pub(crate) fn new(content: &impl IsA<gtk4::Widget>) -> Rc<Self> {
        Rc::new(Loader {
            content: content.clone().upcast(),
            generation: Cell::new(0),
            placeholder: RefCell::new(None),
        })
    }

    /// Start a load of what the page already shows. The content stays up and
    /// usable; `placeholder` runs if the load is still going after
    /// [`LOADING_GRACE`].
    pub(crate) fn refresh(self: &Rc<Self>, placeholder: impl FnOnce() + 'static) -> LoadTicket {
        self.begin(false, placeholder)
    }

    /// Start a load of something else, such as another folder. The old
    /// content stays up but insensitive, since acting on it would act on the
    /// wrong thing; `placeholder` runs if the load is still going after
    /// [`LOADING_GRACE`].
    pub(crate) fn replace(self: &Rc<Self>, placeholder: impl FnOnce() + 'static) -> LoadTicket {
        self.begin(true, placeholder)
    }

    fn begin(self: &Rc<Self>, stale: bool, placeholder: impl FnOnce() + 'static) -> LoadTicket {
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        self.cancel_placeholder();
        self.content.set_sensitive(!stale);
        let loader = Rc::downgrade(self);
        let source = glib::timeout_add_local_once(LOADING_GRACE, move || {
            let Some(loader) = loader.upgrade() else {
                return;
            };
            loader.placeholder.borrow_mut().take();
            loader.content.set_sensitive(true);
            placeholder();
        });
        *self.placeholder.borrow_mut() = Some(source);
        LoadTicket {
            loader: self.clone(),
            generation,
        }
    }

    fn cancel_placeholder(&self) {
        if let Some(source) = self.placeholder.borrow_mut().take() {
            source.remove();
        }
    }
}

/// A load in flight. Dropping it ends the load: the pending loading state is
/// cancelled and the content is usable again, unless a newer load has taken
/// over in the meantime.
pub(crate) struct LoadTicket {
    loader: Rc<Loader>,
    generation: u64,
}

impl LoadTicket {
    /// Whether no newer load has started since this one. A superseded load's
    /// reply is stale and must not be painted.
    pub(crate) fn is_current(&self) -> bool {
        self.loader.generation.get() == self.generation
    }
}

impl Drop for LoadTicket {
    fn drop(&mut self) {
        if self.is_current() {
            self.loader.cancel_placeholder();
            self.loader.content.set_sensitive(true);
        }
    }
}

/// Make `model`, a list of [`BoxedAnyObject`]s holding `T`, hold `items`,
/// replacing only the run of items that changed, in one `items-changed`
/// signal. Rows before and after that run keep their objects, so the views
/// keep their widgets, the selection and the scroll position.
pub(crate) fn replace_items<T: Clone + PartialEq + 'static>(model: &gio::ListStore, items: &[T]) {
    let held = |i: usize, item: &T| {
        model
            .item(i as u32)
            .and_downcast::<BoxedAnyObject>()
            .is_some_and(|object| *object.borrow::<T>() == *item)
    };
    let Some((start, removed, added)) = changed_run(model.n_items() as usize, items, held) else {
        return;
    };
    let added: Vec<BoxedAnyObject> = items[added]
        .iter()
        .cloned()
        .map(BoxedAnyObject::new)
        .collect();
    model.splice(start as u32, removed as u32, &added);
}

/// The one run that differs between an old list of `old_len` items and `new`:
/// where it starts, how many old items it covers, and which new items replace
/// them. `held(i, item)` says whether old item `i` equals `item`. `None` when
/// the lists are equal.
fn changed_run<T>(
    old_len: usize,
    new: &[T],
    held: impl Fn(usize, &T) -> bool,
) -> Option<(usize, usize, std::ops::Range<usize>)> {
    let shared = old_len.min(new.len());
    let prefix = (0..shared).take_while(|&i| held(i, &new[i])).count();
    let suffix = (0..shared - prefix)
        .take_while(|&k| held(old_len - 1 - k, &new[new.len() - 1 - k]))
        .count();
    let removed = old_len - prefix - suffix;
    let added = prefix..new.len() - suffix;
    (removed > 0 || !added.is_empty()).then_some((prefix, removed, added))
}

#[cfg(test)]
mod tests {
    use super::changed_run;

    fn run(old: &[u32], new: &[u32]) -> Option<(usize, usize, Vec<u32>)> {
        changed_run(old.len(), new, |i, item| old[i] == *item)
            .map(|(start, removed, added)| (start, removed, new[added].to_vec()))
    }

    #[test]
    fn equal_lists_change_nothing() {
        assert_eq!(run(&[1, 2, 3], &[1, 2, 3]), None);
        assert_eq!(run(&[], &[]), None);
    }

    #[test]
    fn only_the_differing_middle_is_replaced() {
        assert_eq!(run(&[1, 2, 3, 4], &[1, 9, 4]), Some((1, 2, vec![9])));
        assert_eq!(run(&[1, 2, 3], &[1, 2, 5, 3]), Some((2, 0, vec![5])));
        assert_eq!(run(&[1, 2, 3], &[1, 3]), Some((1, 1, vec![])));
    }

    #[test]
    fn growing_or_shrinking_at_either_end() {
        assert_eq!(run(&[1, 2], &[0, 1, 2]), Some((0, 0, vec![0])));
        assert_eq!(run(&[1, 2], &[1, 2, 3]), Some((2, 0, vec![3])));
        assert_eq!(run(&[1, 2, 3], &[]), Some((0, 3, vec![])));
        assert_eq!(run(&[], &[1]), Some((0, 0, vec![1])));
    }

    #[test]
    fn repeated_items_are_not_counted_twice() {
        // The prefix takes both 1s; the suffix must not reuse them.
        assert_eq!(run(&[1, 1], &[1, 1, 1]), Some((2, 0, vec![1])));
        assert_eq!(run(&[1, 1, 1], &[1, 1]), Some((2, 1, vec![])));
    }
}
