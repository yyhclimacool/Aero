use aero_core::tags::Completion;
use aero_index::TagCount;

use crate::quick_open::fuzzy_score;

pub(super) struct Menu {
    pub trigger: Completion,
    pub generation: u64,
    pub candidates: Option<Vec<TagCount>>,
    pub error: Option<String>,
    pub matches: Vec<usize>,
    pub selected: usize,
}

impl Menu {
    pub fn set_candidates(&mut self, mut candidates: Vec<TagCount>) {
        candidates.sort_by(|a, b| b.note_count.cmp(&a.note_count).then_with(|| a.tag.cmp(&b.tag)));
        self.candidates = Some(candidates);
        self.refilter();
    }

    pub fn refilter(&mut self) {
        self.matches = self.candidates.as_ref().map_or_else(Vec::new, |candidates| {
            candidates
                .iter()
                .enumerate()
                .filter(|(_, c)| fuzzy_score(&self.trigger.query, &c.tag).is_some())
                .take(8)
                .map(|(i, _)| i)
                .collect()
        });
        self.selected = 0;
        if self.candidates.is_some() {
            self.error = None;
        }
    }
}
