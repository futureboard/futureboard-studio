//! A parameter display name that can be replaced after construction.
//!
//! `Param::name` returns a `&str` borrowed from the parameter, and a host may call it from another
//! thread while the editor renames the parameter. A published name is therefore never mutated or
//! freed while the parameter is alive. Every distinct name the parameter has had is kept, and
//! renaming it back to one of them reuses that string, so a title that follows a model choice holds
//! at most one string per model however often it changes. The strings are freed with the
//! parameter.

use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Mutex;

pub(super) struct NameCell {
    /// The current name: one of `names`. Read without locking.
    current: AtomicPtr<Box<str>>,
    /// Every name published so far. The outer `Box` keeps each name where `current` points while
    /// the vector grows. Entries are never removed before the cell is dropped.
    names: Mutex<Vec<Box<Box<str>>>>,
}

impl NameCell {
    pub(super) fn new(name: impl Into<String>) -> Self {
        let first: Box<Box<str>> = Box::new(name.into().into_boxed_str());
        let current = &*first as *const Box<str> as *mut Box<str>;
        Self {
            current: AtomicPtr::new(current),
            names: Mutex::new(vec![first]),
        }
    }

    /// Publish `name`. Allocates only for a name this parameter never had. Not for the audio
    /// thread: it takes a lock.
    pub(super) fn set(&self, name: impl Into<String>) {
        let name = name.into();
        let mut names = self.names.lock().unwrap_or_else(|e| e.into_inner());
        let published = match names.iter().find(|known| {
            let known: &str = known;
            known == name.as_str()
        }) {
            Some(known) => &**known as *const Box<str>,
            None => {
                names.push(Box::new(name.into_boxed_str()));
                &**names.last().expect("just pushed") as *const Box<str>
            }
        };
        self.current
            .store(published as *mut Box<str>, Ordering::Release);
    }

    pub(super) fn get(&self) -> &str {
        // `current` always points at an entry of `names`, which lives as long as `self`.
        unsafe { &**self.current.load(Ordering::Acquire) }
    }

    #[cfg(test)]
    fn stored(&self) -> usize {
        self.names.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::NameCell;

    #[test]
    fn renaming_back_and_forth_keeps_one_string_per_name() {
        let cell = NameCell::new("Green Screamer Drive");
        assert_eq!(cell.get(), "Green Screamer Drive");
        for _ in 0..1000 {
            cell.set("Rats Nest Drive");
            assert_eq!(cell.get(), "Rats Nest Drive");
            cell.set("Green Screamer Drive");
            assert_eq!(cell.get(), "Green Screamer Drive");
        }
        assert_eq!(cell.stored(), 2);
    }

    #[test]
    fn a_name_read_before_a_rename_stays_valid() {
        let cell = NameCell::new("Before");
        let before = cell.get();
        cell.set("After");
        assert_eq!(before, "Before");
        assert_eq!(cell.get(), "After");
    }
}
