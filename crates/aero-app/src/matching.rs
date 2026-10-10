//! Prepared Unicode matching and bounded top-K ranking shared by both pickers.
use aero_core::links::NoteSummary;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Default)]
pub(crate) struct Cancel(pub Arc<AtomicBool>);
impl Drop for Cancel {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

pub(crate) fn prepare(text: &str) -> Vec<char> {
    text.chars().flat_map(char::to_lowercase).collect()
}

pub(crate) fn score(q: &[char], t: &[char]) -> Option<i32> {
    if q.is_empty() {
        return Some(0);
    }
    let (mut score, mut ti, mut previous) = (0, 0, None);
    for &qc in q {
        let pos = t[ti..].iter().position(|&c| c == qc)? + ti;
        score += 10;
        if pos == 0 {
            score += 15;
        } else if !t[pos - 1].is_alphanumeric() {
            score += 8;
        }
        if previous == Some(pos.saturating_sub(1)) && pos > 0 {
            score += 12;
        }
        previous = Some(pos);
        ti = pos + 1;
    }
    Some(score - t.len() as i32 / 8)
}

#[derive(Clone)]
pub(crate) struct Notes(Vec<Arc<Vec<Vec<char>>>>);
impl Notes {
    pub fn new(notes: &[NoteSummary]) -> Self {
        Self(notes.iter().map(|note| Arc::new(Self::targets(note))).collect())
    }
    pub fn reuse(notes: &[NoteSummary], previous: &[NoteSummary], prepared: &Self) -> Self {
        let previous: std::collections::HashMap<_, _> =
            previous.iter().enumerate().map(|(i, note)| (&note.path, (i, note))).collect();
        Self(
            notes
                .iter()
                .map(|note| {
                    if let Some((i, old)) = previous.get(&note.path)
                        && *old == note
                    {
                        prepared.0[*i].clone()
                    } else {
                        Arc::new(Self::targets(note))
                    }
                })
                .collect(),
        )
    }
    fn targets(note: &NoteSummary) -> Vec<Vec<char>> {
        std::iter::once(note.title.as_str())
            .chain(std::iter::once(note.path.as_str()))
            .chain(note.aliases.iter().map(String::as_str))
            .map(prepare)
            .collect()
    }
    pub fn replace(&mut self, index: usize, note: &NoteSummary) {
        self.0[index] = Arc::new(Self::targets(note));
    }
    pub fn push(&mut self, note: &NoteSummary) {
        self.0.push(Arc::new(Self::targets(note)));
    }
    pub fn score(&self, index: usize, query: &[char]) -> Option<i32> {
        self.0[index].iter().filter_map(|target| score(query, target)).max()
    }
}

pub(crate) fn top_k<'a>(
    count: usize,
    limit: usize,
    cancelled: &AtomicBool,
    mut rank: impl FnMut(usize) -> Option<(i32, &'a str)>,
) -> Vec<usize> {
    let mut best = BinaryHeap::with_capacity(limit);
    for index in 0..count {
        if cancelled.load(Ordering::Relaxed) {
            return Vec::new();
        }
        let Some((score, path)) = rank(index) else { continue };
        let ranked = (score, Reverse(path), Reverse(index));
        if best.len() < limit {
            best.push(Reverse(ranked));
        } else if best.peek().is_some_and(|worst| ranked > worst.0) {
            *best.peek_mut().unwrap() = Reverse(ranked);
        }
    }
    best.into_sorted_vec().into_iter().map(|Reverse((_, _, Reverse(i)))| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_ranking_matches_full_sort_and_cancels_during_the_loop() {
        let paths: Vec<_> = (0..10000).map(|i| format!("note-{i:05}.md")).collect();
        let rank = |i: usize| ((i % 23) as i32, paths[i].as_str());
        let mut expected: Vec<_> = (0..paths.len()).collect();
        expected.sort_by(|&a, &b| rank(b).0.cmp(&rank(a).0).then(paths[a].cmp(&paths[b])));
        let cancelled = AtomicBool::new(false);
        let actual = top_k(paths.len(), 12, &cancelled, |i| Some(rank(i)));
        assert_eq!(actual, expected[..12]);
        let mut visited = 0;
        assert!(
            top_k(paths.len(), 12, &cancelled, |i| {
                visited += 1;
                if i == 17 {
                    cancelled.store(true, Ordering::Relaxed);
                }
                Some(rank(i))
            })
            .is_empty()
        );
        assert_eq!(visited, 18, "cancellation stops active computation");
    }

    #[test]
    fn prepared_matching_preserves_unicode_aliases_and_word_boundaries() {
        assert_eq!(score(&prepare("雪İ"), &prepare("雪İstanbul")), Some(68));
        assert!(
            score(&prepare("rs"), &prepare("Rust source")).unwrap()
                > score(&prepare("rs"), &prepare("errors")).unwrap()
        );
        let guard = Cancel::default();
        let flag = guard.0.clone();
        drop(guard);
        assert!(flag.load(Ordering::Relaxed));
    }
}
