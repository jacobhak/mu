//! A `Select` whose rows each carry a value, so a choice comes back as the value and not as an
//! index into a list that has to be kept in step with the rows.

use crossterm::event::KeyEvent;

use crate::widgets::select::{Item, Select, SelectResult};

/// Something that has a `Select` to draw and type into.
pub trait HasSelect {
    fn sel(&self) -> &Select;
    fn sel_mut(&mut self) -> &mut Select;
}

impl HasSelect for Select {
    fn sel(&self) -> &Select {
        self
    }

    fn sel_mut(&mut self) -> &mut Select {
        self
    }
}

impl<T> HasSelect for Picker<T> {
    fn sel(&self) -> &Select {
        &self.sel
    }

    fn sel_mut(&mut self) -> &mut Select {
        &mut self.sel
    }
}

pub struct Picker<T> {
    pub sel: Select,
    values: Vec<T>,
}

/// What a key did to a picker.
#[derive(Debug, PartialEq)]
pub enum Pick<T> {
    /// Still open.
    Pending,
    /// A row was chosen: its value, or `None` for a row that carries none (a loading placeholder).
    Chosen(Option<T>),
    /// Closed without a choice.
    Dismissed,
}

impl<T> Picker<T> {
    /// The rows with their values; `select` builds the `Select` over the rows' items.
    pub fn from_rows(rows: Vec<(Item, T)>, select: impl FnOnce(Vec<Item>) -> Select) -> Self {
        let (items, values): (Vec<_>, Vec<_>) = rows.into_iter().unzip();
        Picker { sel: select(items), values }
    }

    /// A picker with no values yet, which shows `items` (a loading row) until it is replaced.
    pub fn placeholder(items: Vec<Item>, select: impl FnOnce(Vec<Item>) -> Select) -> Self {
        Picker { sel: select(items), values: Vec::new() }
    }

    /// Whether this is a placeholder, or an empty list.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    #[cfg(test)]
    pub fn values(&self) -> &[T] {
        &self.values
    }

    /// The value of the highlighted row.
    pub fn highlighted(&self) -> Option<&T> {
        self.sel.highlighted().and_then(|i| self.values.get(i))
    }
}

impl<T: Clone> Picker<T> {
    pub fn on_key(&mut self, k: KeyEvent) -> Pick<T> {
        match self.sel.on_key(k) {
            SelectResult::Pending => Pick::Pending,
            SelectResult::Chosen(rows) => Pick::Chosen(rows.first().and_then(|&i| self.values.get(i)).cloned()),
            _ => Pick::Dismissed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyCode;
    use mu_testutil::key;

    fn item(label: &str) -> Item {
        Item { label: label.into(), detail: String::new() }
    }

    fn picker() -> Picker<u32> {
        Picker::from_rows(vec![(item("a"), 10), (item("b"), 20)], |items| Select::new("t", items))
    }

    #[test]
    fn a_choice_returns_the_value_of_its_row() {
        let mut p = picker();
        assert_eq!(p.highlighted(), Some(&10));
        assert_eq!(p.on_key(key(KeyCode::Down)), Pick::Pending);
        assert_eq!(p.highlighted(), Some(&20));
        assert_eq!(p.on_key(key(KeyCode::Enter)), Pick::Chosen(Some(20)));
    }

    #[test]
    fn escape_dismisses() {
        assert_eq!(picker().on_key(key(KeyCode::Esc)), Pick::Dismissed);
    }

    #[test]
    fn a_placeholder_chooses_nothing() {
        let mut p: Picker<u32> = Picker::placeholder(vec![item("Loading…")], |items| Select::new("t", items));
        assert!(p.is_empty());
        assert_eq!(p.on_key(key(KeyCode::Enter)), Pick::Chosen(None));
    }
}
