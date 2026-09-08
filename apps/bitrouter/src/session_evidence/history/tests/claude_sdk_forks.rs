use super::*;
use std::collections::BTreeMap;
use std::path::{Component, PathBuf};
use tokio::io::AsyncWriteExt;

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key].as_str().context("capture string")
}

fn path(root: &Path, value: &Value, key: &str) -> Result<PathBuf> {
    let relative = Path::new(text(value, key)?);
    ensure!(
        relative
            .components()
            .all(|part| matches!(part, Component::Normal(_))),
        "capture path"
    );
    Ok(root.join(relative))
}

async fn json(path: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&tokio::fs::read(path).await?)?)
}

fn texts(content: &Value) -> Result<Vec<&str>> {
    if let Some(text) = content.as_str() {
        return Ok(vec![text]);
    }
    content
        .as_array()
        .context("message content")?
        .iter()
        .map(|block| {
            ensure!(
                block["type"] == "text",
                "fixture unexpectedly used a nontext block"
            );
            text(block, "text")
        })
        .collect()
}

fn context_rows<'a>(
    projection: &Projection,
    records: &'a [StoredRecord],
) -> Result<Vec<&'a Value>> {
    let by_id: BTreeMap<_, _> = records.iter().map(|record| (&record.id, record)).collect();
    projection
        .effective_context
        .iter()
        .map(|reference| {
            ensure!(reference.pointer.is_empty(), "native message reference");
            Ok(&by_id
                .get(&reference.record_id)
                .context("context record")?
                .input
                .raw)
        })
        .collect()
}

fn check_request(
    projection: &Projection,
    records: &[StoredRecord],
    request: &Value,
    label: &str,
) -> Result<()> {
    let messages = context_rows(projection, records)?;
    let mut expected = vec![];
    for message in messages {
        let role = text(message, "type")?;
        if matches!(role, "user" | "assistant") {
            for text in texts(&message["message"]["content"])? {
                expected.push((role.to_owned(), text.trim().to_owned()));
            }
        }
    }
    expected.push(("user".into(), format!("Continue {label}.")));
    let mut actual = vec![];
    let mut date_reminders = 0;
    for message in request["body"]["messages"]
        .as_array()
        .context("model request messages")?
    {
        let role = text(message, "role")?;
        for text in texts(&message["content"])? {
            // Fixture-specific request decoration, not a general prompt
            // normalizer. Keep every other dialogue block in exact order.
            if text.starts_with("<system-reminder>\nAs you answer the user's questions,") {
                ensure!(
                    role == "user" && text.trim_end().ends_with("</system-reminder>"),
                    "date reminder"
                );
                date_reminders += 1;
            } else {
                actual.push((role.to_owned(), text.trim().to_owned()));
            }
        }
    }
    assert_eq!(date_reminders, 1);
    assert_eq!(actual, expected, "native request for {label}");
    Ok(())
}

async fn records(store: &EvidenceStore, range: &SourceRange) -> Result<Vec<StoredRecord>> {
    ensure!(
        range.end - range.start <= MAX_RECORDS as u64,
        "capture range"
    );
    let mut all = vec![];
    let mut start = range.start;
    while start < range.end {
        let end = (start + RECORD_PAGE_SIZE).min(range.end);
        let page = store
            .records(&SourceRange {
                start,
                end,
                ..range.clone()
            })
            .await?;
        ensure!(page.len() as u64 == end - start, "missing source records");
        all.extend(page);
        start = end;
    }
    Ok(all)
}

async fn check_history(
    resolver: &HistoryResolver,
    node: &NodeKey,
    reader: &Value,
) -> Result<(Projection, SourceRange, Vec<StoredRecord>)> {
    let history = resolver.resolve(node.clone()).await?;
    assert!(history.variants.is_empty());
    assert!(history.gaps.is_empty(), "{:?}", history.gaps);
    // A message copy claim alone must not invent a verified parent cut.
    assert!(history.parent.is_none());
    assert!(history.edge.is_none());
    let projection = history.projection.context("projection")?;
    assert!(projection.gaps.is_empty(), "{:?}", projection.gaps);
    let range = history
        .source
        .and_then(|source| source.range)
        .context("source range")?;
    let all = records(&resolver.store, &range).await?;
    let by_id: BTreeMap<_, _> = all.iter().map(|record| (&record.id, record)).collect();
    let context = context_rows(&projection, &all)?;
    let ids = context
        .iter()
        .map(|raw| text(raw, "uuid"))
        .collect::<Result<Vec<_>>>()?;
    assert_eq!(ids.iter().collect::<BTreeSet<_>>().len(), ids.len());
    // getSessionMessages filters display metadata after constructing the
    // native chain (SHe in the published SDK). Keep that metadata in our
    // context and check it separately against the actual model request.
    // https://www.npmjs.com/package/@anthropic-ai/claude-agent-sdk/v/0.3.257
    let actual = context
        .into_iter()
        .filter(|raw| {
            matches!(raw["type"].as_str(), Some("user" | "assistant" | "system"))
                && raw["isMeta"] != true
                && raw["isSidechain"] != true
                && raw.get("teamName").is_none_or(|value| value.is_null())
        })
        .map(|raw| text(raw, "uuid"))
        .collect::<Result<Vec<_>>>()?;
    let expected = reader["messages"]
        .as_array()
        .context("SDK messages")?
        .iter()
        .map(|message| text(message, "uuid"))
        .collect::<Result<Vec<_>>>()?;
    assert_eq!(actual, expected);
    assert_eq!(actual.iter().collect::<BTreeSet<_>>().len(), actual.len());
    assert_eq!(
        projection.raw_record_ids,
        all.iter()
            .map(|record| record.id.clone())
            .collect::<Vec<_>>()
    );
    for (id, origin) in &projection.claude_fork_origins {
        let raw = &by_id.get(id).context("copy record")?.input.raw;
        assert_eq!(raw["uuid"], origin.message_uuid);
        assert_eq!(raw["forkedFrom"]["sessionId"], origin.source_session_id);
        assert_eq!(raw["forkedFrom"]["messageUuid"], origin.source_message_uuid);
        assert_ne!(origin.message_uuid, origin.source_message_uuid);
        assert_ne!(origin.source_session_id, node.native_id);
    }
    Ok((projection, range, all))
}

#[tokio::test]
#[ignore = "requires an isolated original Claude SDK fork capture"]
async fn captured_claude_sdk_forks_match_sdk_reader_and_native_requests() -> Result<()> {
    let capture = PathBuf::from(std::env::var("BITROUTER_TEST_CLAUDE_SDK_FORK_CAPTURE")?);
    let metadata = json(&capture.join("sdk-forks.json")).await?;
    ensure!(
        metadata["schema"] == "claude-sdk-fork-capture/1"
            && metadata["sdk_version"] == "0.3.257"
            && metadata["version"] == "2.1.220 (Claude Code)",
        "capture producer contract"
    );
    let requests = json(&capture.join("requests.json")).await?;
    let cases = metadata["cases"].as_array().context("fork cases")?;
    assert_eq!(
        cases
            .iter()
            .map(|case| text(case, "label"))
            .collect::<Result<Vec<_>>>()?,
        ["sdk-full", "sdk-nested", "sdk-bounded"]
    );
    for case in cases {
        let label = text(case, "label")?;
        let directory = tempfile::tempdir()?;
        let native = directory.path().join("projects");
        tokio::fs::create_dir(&native).await?;
        let node = NodeKey {
            harness: Harness::ClaudeCode,
            namespace: text(&metadata, "namespace")?.into(),
            native_id: text(case, "session")?.into(),
            agent_id: None,
        };
        let native_path = native.join(format!("{}.jsonl", node.native_id));
        let mut bytes = tokio::fs::read(path(&capture, &case["before"], "transcript")?).await?;
        tokio::fs::write(&native_path, &bytes).await?;
        let url = format!(
            "sqlite://{}",
            directory.path().join("evidence.db").display()
        );
        let db = crate::db::connect(&url).await?;
        crate::db::run_migrations(&db).await?;
        let make = |db| -> Result<HistoryResolver> {
            let store = EvidenceStore::new(db, "local")?;
            Ok(HistoryResolver::new(
                store.clone(),
                NativeCollector::new(
                    store,
                    NativeRoot {
                        harness: Harness::ClaudeCode,
                        namespace: node.namespace.clone(),
                        directory: native.clone(),
                    },
                )?,
            ))
        };
        let resolver = make(db.clone())?;
        let reader = json(&path(&capture, &case["before"], "reader")?).await?;
        let request_index = case["request_index"].as_u64().context("request index")? as usize;
        let (before, frozen_range, frozen_records) =
            check_history(&resolver, &node, &reader).await?;
        check_request(
            &before,
            &frozen_records,
            requests
                .as_array()
                .context("requests")?
                .get(request_index)
                .context("native request")?,
            label,
        )?;
        assert!(!before.claude_fork_origins.is_empty());
        assert_eq!(
            before.claude_unapplied_compactions.len(),
            match label {
                "sdk-full" => 2,
                "sdk-nested" => 3,
                _ => 1,
            }
        );
        let parent_bytes = tokio::fs::read(path(&capture, case, "parent_transcript")?).await?;
        let parent_rows = std::str::from_utf8(&parent_bytes)?
            .lines()
            .map(serde_json::from_str::<Value>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let cut = if let Some(uuid) = case["up_to"].as_str() {
            assert!(
                before
                    .claude_fork_origins
                    .values()
                    .any(|copy| copy.source_message_uuid == uuid)
            );
            parent_rows
                .iter()
                .position(|row| row["uuid"] == uuid)
                .context("fork cut")?
                + 1
        } else {
            parent_rows.len()
        };
        let parent_ids: BTreeSet<_> = parent_rows[..cut]
            .iter()
            .filter_map(|row| row["uuid"].as_str())
            .collect();
        for copy in before.claude_fork_origins.values() {
            assert_eq!(copy.source_session_id, text(case, "parent")?);
            assert!(parent_ids.contains(copy.source_message_uuid.as_str()));
        }
        let mut latest = before.clone();
        for stage in ["after", "compacted"] {
            if case[stage].is_null() {
                continue;
            }
            let next = tokio::fs::read(path(&capture, &case[stage], "transcript")?).await?;
            ensure!(
                next.starts_with(&bytes),
                "native source did not append at {label}/{stage}"
            );
            let mut file = tokio::fs::OpenOptions::new()
                .append(true)
                .open(&native_path)
                .await?;
            file.write_all(&next[bytes.len()..]).await?;
            file.flush().await?;
            drop(file);
            bytes = next;
            let reader = json(&path(&capture, &case[stage], "reader")?).await?;
            latest = check_history(&resolver, &node, &reader).await?.0;
            assert_eq!(
                latest.claude_unapplied_compactions.len(),
                before.claude_unapplied_compactions.len()
            );
            assert_eq!(
                records(&resolver.store, &frozen_range).await?,
                frozen_records
            );
        }
        tokio::fs::remove_file(&native_path).await?;
        drop(resolver);
        db.close().await?;
        let reopened = make(crate::db::connect(&url).await?)?;
        let restored = reopened.resolve(node.clone()).await?;
        assert!(restored.variants.is_empty());
        assert_eq!(
            restored.gaps,
            BTreeSet::from(["native_source_file_unavailable".into()])
        );
        assert_eq!(restored.projection.as_ref(), Some(&latest));
        assert_eq!(
            records(&reopened.store, &frozen_range).await?,
            frozen_records
        );
        let mut replay = Projector::new(
            node,
            super::super::super::types::SourceFormat::ClaudeTranscript,
        )?;
        for record in &frozen_records {
            replay.push(record)?;
        }
        assert_eq!(replay.finish(), before);
    }
    Ok(())
}
