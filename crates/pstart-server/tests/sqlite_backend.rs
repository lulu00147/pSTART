use pstart_core::VolumeId;
use pstart_index::VolumeMeta;
use pstart_server::IndexPipeline;
use tempfile::TempDir;

#[test]
fn pipeline_without_sqlite_returns_empty_search() {
    let directory = TempDir::new().unwrap();
    let meta = VolumeMeta::new(VolumeId::new("C:").unwrap(), "System", "NTFS").unwrap();
    let pipeline =
        IndexPipeline::open(meta, "C:\\", &directory.path().join("index.db"), 1).unwrap();
    assert_eq!(pipeline.indexed_files(), 0);
    assert!(pipeline.search("alpha", 10).hits.is_empty());
}
