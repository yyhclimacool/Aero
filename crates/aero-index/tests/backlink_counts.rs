use std::sync::Arc;

use aero_core::{Vault, VaultPath, index::MemoryFullTextIndex};
use aero_index::{Indexer, Meta};

#[test]
fn backlink_totals_and_block_filters_are_independent_of_the_page_limit() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("target.md"), "body ^one\n\nsecond ^two\n\n[[target]]\n").unwrap();
    let source = format!(
        "{}\n[[target#^one]] [[target#^one]] [[target#^two]] [[target#heading]]\n",
        "[[target]] ".repeat(205)
    );
    std::fs::write(dir.path().join("source.md"), &source).unwrap();
    let vault = Arc::new(Vault::open(dir.path()).unwrap());
    let mut index = Indexer::with_parts(
        vault,
        Meta::open(&dir.path().join("meta.sqlite")).unwrap(),
        Box::new(MemoryFullTextIndex::default()),
    );
    assert!(index.sync_all(|_, _| {}).errors.is_empty());
    let target = VaultPath::new("target.md").unwrap();
    let all = index.backlinks(&target, None, 200).unwrap();
    assert_eq!(all.total, 209);
    assert_eq!(all.matched, 209);
    assert_eq!(all.hits.len(), 200);
    let block = index.backlinks(&target, Some("^one"), 1).unwrap();
    assert_eq!(block.total, 209);
    assert_eq!(block.matched, 2);
    assert_eq!(block.hits.len(), 1);
    assert_eq!(&source[block.hits[0].range.clone()], "[[target#^one]]");
    let missing = index.backlinks(&target, Some("^missing"), 200).unwrap();
    assert_eq!(missing.total, 209);
    assert_eq!(missing.matched, 0);
    assert!(missing.hits.is_empty());
    let source_path = VaultPath::new("source.md").unwrap();
    std::fs::write(dir.path().join("source.md"), "[[target#^two]]\n").unwrap();
    index.index_note(&source_path).unwrap();
    let updated = index.backlinks(&target, Some("^one"), 200).unwrap();
    assert_eq!(updated.total, 1);
    assert_eq!(updated.matched, 0);
    std::fs::remove_file(dir.path().join("source.md")).unwrap();
    index.remove_note(&source_path).unwrap();
    assert_eq!(index.backlinks(&target, None, 200).unwrap().total, 0);
}
