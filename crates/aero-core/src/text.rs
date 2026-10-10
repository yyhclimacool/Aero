//! 文本层：Rope 与编辑操作。
//!
//! `Rope` 直接就是 ropey 2.0 的类型（ADR 0004），与 gpui-base 的输入控件同型，
//! 所有偏移都是 UTF-8 字节偏移。

use std::ops::Range;

pub use ropey::{Rope, RopeSlice};

/// 本项目内部统一用 LF 计行（文件里的 CRLF 在读入时归一化，见 [`SourceText`]）。
pub const LINE_TYPE: ropey::LineType = ropey::LineType::LF;

/// 一次原子编辑：把 `range` 内的字节替换为 `text`。插入即 `range` 为空，删除即 `text` 为空。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub range: Range<usize>,
    pub text: String,
}

impl Edit {
    pub fn insert(at: usize, text: impl Into<String>) -> Self {
        Edit { range: at..at, text: text.into() }
    }

    pub fn delete(range: Range<usize>) -> Self {
        Edit { range, text: String::new() }
    }

    pub fn replace(range: Range<usize>, text: impl Into<String>) -> Self {
        Edit { range, text: text.into() }
    }

    /// 替换后新内容的长度
    pub fn new_len(&self) -> usize {
        self.text.len()
    }

    /// 编辑造成的长度变化（新 − 旧）
    pub fn delta(&self) -> isize {
        self.text.len() as isize - self.range.len() as isize
    }

    /// 编辑后被替换区间在新文本中的位置
    pub fn new_range(&self) -> Range<usize> {
        self.range.start..self.range.start + self.text.len()
    }
}

/// 编辑非法的原因。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EditError {
    #[error("edit range {start}..{end} exceeds text length {len}")]
    OutOfBounds { start: usize, end: usize, len: usize },
    #[error("edit range {start}..{end} does not fall on char boundaries")]
    NotCharBoundary { start: usize, end: usize },
}

/// 检查一次编辑是否落在合法的字节区间上。
pub fn check_edit(rope: &Rope, edit: &Edit) -> Result<(), EditError> {
    let len = rope.len();
    let Range { start, end } = edit.range;
    if start > end || end > len {
        return Err(EditError::OutOfBounds { start, end, len });
    }
    if !rope.is_char_boundary(start) || !rope.is_char_boundary(end) {
        return Err(EditError::NotCharBoundary { start, end });
    }
    Ok(())
}

/// 把编辑应用到 rope，返回能恰好撤销它的逆编辑。调用前必须通过 [`check_edit`]。
pub fn apply_edit(rope: &mut Rope, edit: &Edit) -> Edit {
    let removed = rope.slice(edit.range.clone()).to_string();
    if !edit.range.is_empty() {
        rope.remove(edit.range.clone());
    }
    if !edit.text.is_empty() {
        rope.insert(edit.range.start, &edit.text);
    }
    Edit { range: edit.new_range(), text: removed }
}

/// Text positions changed by an edit, without retaining inserted or removed text.
#[derive(Clone, Debug)]
pub struct EditSummary {
    pub range: Range<usize>,
    pub inserted: usize,
}

impl From<&Edit> for EditSummary {
    fn from(edit: &Edit) -> Self {
        Self { range: edit.range.clone(), inserted: edit.text.len() }
    }
}

impl EditSummary {
    pub fn shift_range(&self, range: &mut Range<usize>) {
        let delta = self.inserted as isize - self.range.len() as isize;
        range.start = crate::anchors::shift(
            range.start,
            crate::anchors::Bias::Left,
            self.range.start,
            self.range.end,
            self.inserted,
            delta,
        );
        range.end = crate::anchors::shift(
            range.end,
            crate::anchors::Bias::Right,
            self.range.start,
            self.range.end,
            self.inserted,
            delta,
        );
    }
}

pub fn word_count(text: &str) -> usize {
    count_words(text.chars(), false)
}

fn in_word(character: char) -> bool {
    character.is_alphanumeric() && !is_cjk(character)
}

fn is_cjk(character: char) -> bool {
    matches!(character as u32,
        0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0x20000..=0x2A6DF | 0x3040..=0x30FF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF)
}

fn count_words(characters: impl Iterator<Item = char>, mut previous_in_word: bool) -> usize {
    let mut count = 0;
    for character in characters {
        let current_in_word = in_word(character);
        if is_cjk(character) || (current_in_word && !previous_in_word) {
            count += 1;
        }
        previous_in_word = current_in_word;
    }
    count
}

pub(crate) fn edited_word_count(rope: &Rope, edit: &Edit, previous: usize) -> usize {
    let left_in_word = rope.chars_at(edit.range.start).reversed().next().is_some_and(in_word);
    let right = rope.slice(edit.range.end..).chars().next();
    // A replacement only changes its own contributions and the following
    // character's word boundary. A preceding character supplies the context.
    let removed = count_words(rope.slice(edit.range.clone()).chars().chain(right), left_in_word);
    let inserted = count_words(edit.text.chars().chain(right), left_in_word);
    previous - removed + inserted
}

/// 文件的行尾风格。内部文本一律 LF，写回时还原。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LineEnding {
    #[default]
    Lf,
    CrLf,
}

/// 从磁盘读入的文本及其需要在写回时还原的表层特征（5.5：保留 BOM 与行尾风格）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceText {
    pub text: String,
    pub bom: bool,
    pub line_ending: LineEnding,
}

impl SourceText {
    /// 解码文件字节：去 BOM、CRLF → LF。不是合法 UTF-8 时报错，绝不有损替换。
    pub fn decode(bytes: &[u8]) -> Result<Self, std::str::Utf8Error> {
        let (bom, body) = match bytes.strip_prefix(b"\xEF\xBB\xBF") {
            Some(rest) => (true, rest),
            None => (false, bytes),
        };
        let s = std::str::from_utf8(body)?;
        let line_ending = if s.contains("\r\n") { LineEnding::CrLf } else { LineEnding::Lf };
        let text = match line_ending {
            LineEnding::CrLf => s.replace("\r\n", "\n"),
            LineEnding::Lf => s.to_owned(),
        };
        Ok(SourceText { text, bom, line_ending })
    }

    /// 新建文件的默认形态：无 BOM、LF。
    pub fn fresh(text: impl Into<String>) -> Self {
        SourceText { text: text.into(), bom: false, line_ending: LineEnding::Lf }
    }

    /// 按原表层特征编码回字节。
    pub fn encode(text: &str, bom: bool, line_ending: LineEnding) -> Vec<u8> {
        let mut out = Vec::with_capacity(text.len() + 3 + text.len() / 32);
        if bom {
            out.extend_from_slice(b"\xEF\xBB\xBF");
        }
        match line_ending {
            LineEnding::Lf => out.extend_from_slice(text.as_bytes()),
            LineEnding::CrLf => {
                for (i, line) in text.split('\n').enumerate() {
                    if i > 0 {
                        out.extend_from_slice(b"\r\n");
                    }
                    out.extend_from_slice(line.as_bytes());
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_and_undo() {
        let mut r = Rope::from_str("héllo wörld");
        let e = Edit::replace(1..3, "e"); // 'é' 是 2 字节
        check_edit(&r, &e).unwrap();
        let inv = apply_edit(&mut r, &e);
        assert_eq!(r.to_string(), "hello wörld");
        apply_edit(&mut r, &inv);
        assert_eq!(r.to_string(), "héllo wörld");
    }

    #[test]
    fn rejects_mid_char_edit() {
        let r = Rope::from_str("é");
        assert_eq!(
            check_edit(&r, &Edit::insert(1, "x")),
            Err(EditError::NotCharBoundary { start: 1, end: 1 })
        );
        assert_eq!(
            check_edit(&r, &Edit::delete(0..5)),
            Err(EditError::OutOfBounds { start: 0, end: 5, len: 2 })
        );
    }

    #[test]
    fn source_text_roundtrip() {
        let bytes = b"\xEF\xBB\xBF# a\r\n\r\nb\r\n";
        let s = SourceText::decode(bytes).unwrap();
        assert_eq!(s.text, "# a\n\nb\n");
        assert!(s.bom);
        assert_eq!(s.line_ending, LineEnding::CrLf);
        assert_eq!(SourceText::encode(&s.text, s.bom, s.line_ending), bytes);
    }

    #[test]
    fn edited_word_counts_preserve_unicode_and_word_boundaries() {
        for source in ["", "hello world", "你好école１２3", "foo-bar", "a\n中b", "かな한글", "e\u{301}cole"]
        {
            let boundaries: Vec<_> =
                source.char_indices().map(|(index, _)| index).chain([source.len()]).collect();
            let rope = Rope::from_str(source);
            for &start in &boundaries {
                for &end in boundaries.iter().filter(|&&end| end >= start) {
                    for inserted in ["", " ", "中文", "é", "hello world", "\n"] {
                        let edit = Edit::replace(start..end, inserted);
                        let mut expected = source.to_owned();
                        expected.replace_range(start..end, inserted);
                        assert_eq!(
                            edited_word_count(&rope, &edit, word_count(source)),
                            word_count(&expected),
                            "{source:?} {edit:?}"
                        );
                    }
                }
            }
        }
    }
}
