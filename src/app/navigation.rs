//! Section and back/forward history for the app rail. Transient UI state:
//! never persisted.

use crate::shared::model::Section;

pub(super) struct Navigation {
    pub(super) section: Section,
    pub(super) history: Vec<Section>,
    pub(super) index: usize,
}

impl Navigation {
    pub(super) fn new(section: Section) -> Self {
        Self {
            section,
            history: vec![section],
            index: 0,
        }
    }

    pub(super) fn can_go_back(&self) -> bool {
        self.index > 0
    }

    pub(super) fn can_go_forward(&self) -> bool {
        self.index + 1 < self.history.len()
    }

    /// Records a visit to a different section, dropping any forward history.
    pub(super) fn push(&mut self, section: Section) {
        self.history.truncate(self.index + 1);
        self.history.push(section);
        self.index += 1;
        self.section = section;
    }

    pub(super) fn step_back(&mut self) {
        self.index -= 1;
        self.section = self.history[self.index];
    }

    pub(super) fn step_forward(&mut self) {
        self.index += 1;
        self.section = self.history[self.index];
    }

    pub(super) fn reset(&mut self) {
        *self = Self::new(Section::Chat);
    }
}
