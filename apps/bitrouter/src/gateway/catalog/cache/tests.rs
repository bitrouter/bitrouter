use super::super::tests::document;
use super::*;
use registry_json as serde_json;

#[test]
fn complete_raw_cache_roundtrips_and_exact_sources_do_not_share_entries() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let cache = DiskCache::at(root.path());
    let mut raw = document("native");
    raw["future"] = json!({"$serde_json::private::Number":"opaque","sibling":true});
    let data = super::super::decode::from_slice(&serde_json::to_vec(&raw)?)?;
    let snapshot = CatalogSnapshot {
        fetched_at: 42,
        data,
    };
    let source = "https://example.com/registry.json?revision=a";
    cache.save(source, &snapshot)?;
    let loaded = cache
        .load(source)?
        .ok_or_else(|| anyhow::anyhow!("cache missing"))?;
    assert_eq!(loaded.fetched_at, 42);
    assert_eq!(loaded.data.document, Some(raw));
    for other in [
        "https://example.com/registry.json?revision=b",
        "https://example.com/registry.json?revision=a/",
        "https://other.example/registry.json?revision=a",
    ] {
        assert!(cache.load(other)?.is_none());
    }
    let before = fs::read(cache.path(source))?;
    let mut bad = snapshot;
    bad.data
        .document
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("no document"))?["version"] = json!(2);
    assert!(cache.save(source, &bad).is_err());
    assert_eq!(fs::read(cache.path(source))?, before);
    fs::write(
        cache.path(source),
        b"{\"cache_version\":1,\"source\":\"https://wrong.example\"}\n{}",
    )?;
    assert!(cache.load(source)?.is_none());
    Ok(())
}
#[test]
fn malformed_checked_at_does_not_erase_an_otherwise_valid_document() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let cache = DiskCache::at(root.path());
    let source = "https://fixture.example/v1.json";
    fs::write(
        cache.path(source),
        format!(
            "{}\n{}",
            json!({"cache_version":1,"source":source,"checked_at":"invalid"}),
            document("kept")
        ),
    )?;
    let snapshot = cache
        .load(source)?
        .ok_or_else(|| anyhow::anyhow!("document discarded"))?;
    assert_eq!(snapshot.fetched_at, 0);
    let mut catalog = Catalog::new(source);
    catalog.seed(snapshot);
    assert!(!catalog.is_fresh(TTL));
    Ok(())
}
