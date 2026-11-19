use pstart_core::{Frn, VolumeId};
use pstart_index::{FileRecord, MemoryIndex};

fn record(frn: u64, parent: u64, name: &str, usn: u64) -> FileRecord {
    FileRecord {
        frn,
        parent_frn: parent,
        name: name.into(),
        attributes: 0,
        is_dir: false,
        last_usn: usn,
    }
}

fn index() -> MemoryIndex {
    let mut index = MemoryIndex::new(VolumeId::new("C:").unwrap(), "C:\\", Frn::ROOT, 64).unwrap();
    for value in [
        record(5, 5, ".", 1),
        record(10, 5, "Users", 2),
        record(11, 10, "Documents", 3),
        record(12, 11, "Report.docx", 4),
        record(13, 11, "annual-report.txt", 5),
        record(14, 11, "myreport.txt", 6),
        record(15, 11, "Report", 7),
    ] {
        assert!(index.upsert(value).unwrap());
    }
    index
}

fn names(index: &MemoryIndex, query: &str) -> Vec<String> {
    index
        .search(query, 10)
        .hits
        .into_iter()
        .map(|hit| hit.item.name)
        .collect()
}

#[test]
fn fixed_query_set_keeps_order() {
    let index = index();
    assert_eq!(
        names(&index, "report"),
        vec!["Report", "Report.docx", "annual-report.txt", "myreport.txt"]
    );
    assert_eq!(names(&index, "REPORT"), names(&index, "report"));
    assert_eq!(names(&index, "docx"), vec!["Report.docx"]);
    assert_eq!(names(&index, "annual"), vec!["annual-report.txt"]);
    assert!(names(&index, "no-such-name").is_empty());
}
