use super::*;
fn fixture() -> Result<tempfile::TempDir> {
    let root = tempfile::tempdir()?;
    for dir in ["registry/models", "registry/providers"] {
        fs::create_dir_all(root.path().join(dir))?;
    }
    let values = [
        (
            "registry/registry.yaml",
            json!({"version":1,"spec_revision":SPEC_REVISION,"as_of":"2026-10-22T00:00:00Z","future":{"opaque":true},"identity_source":{"kind":"openrouter","request":{"url":"https://openrouter.ai/api/v1/models?output_modalities=all","auth":{"kind":"none"}}}}),
        ),
        (
            "registry/models/org.yaml",
            json!([{"id":"org/model","curation":{"reason":"fixture","source_url":"https://example.com/evidence","checked_at":"2026-10-22T00:00:00Z"}}]),
        ),
        (
            "registry/providers/test.yaml",
            json!({"name":"test","status":"active","billing":"usage","api_base":"https://example.com/v1","auth":{"kind":"none"},"models":[{"provider_model_id":"Native/A","canonical_id":"org/model","status":"active","interfaces":[{"protocol":"messages","operation":"generation"}],"future":{"private":{"$serde_json::private::Number":"opaque","other":true}}}]}),
        ),
        (
            "registry/model-identities.yaml",
            json!([{"canonical_id":"org/model","source_url":"https://openrouter.ai/api/v1/models?output_modalities=all","observed_at":"2026-10-22T00:00:00Z","bindings":[{"provider":"test","provider_model_id":"Native/A","source_url":"https://example.com/evidence","verified_at":"2026-10-22T00:00:00Z"}]}]),
        ),
        (
            "registry/model-discovery.yaml",
            json!({"families":[],"retired_models":[]}),
        ),
    ];
    for (path, value) in values {
        atomic_write(&root.path().join(path), source::render(&value)?.as_bytes())?;
    }
    Ok(root)
}
#[test]
fn deterministic_build_preserves_raw_data_and_recommendation_removal_never_deletes_inventory()
-> Result<()> {
    let root = fixture()?;
    build(root.path(), false)?;
    build(root.path(), true)?;
    let before = fs::read(root.path().join("dist/registry/v1/registry.json"))?;
    let document = Document::parse(&before, &ReaderPolicy::default())?;
    assert_eq!(document.value()["future"], json!({"opaque":true}));
    assert_eq!(
        document.providers()[0]["models"][0]["future"]["private"]["$serde_json::private::Number"],
        "opaque"
    );
    atomic_write(&root.path().join("registry/models/org.yaml"), b"[]\n")?;
    build(root.path(), false)?;
    let after = Document::parse(
        &fs::read(root.path().join("dist/registry/v1/registry.json"))?,
        &ReaderPolicy::default(),
    )?;
    assert!(after.models().is_empty());
    assert_eq!(after.providers(), document.providers());
    Ok(())
}
#[test]
fn global_identity_gate_scans_unchanged_inactive_and_noncurated_bindings() -> Result<()> {
    let root = fixture()?;
    let sources = Sources::load(root.path())?;
    let document = sources.document()?;
    let ids = identity_ids(&json!({"data":[{"id":"org/model"}]}))?;
    validate_bindings(&document, &sources.files[&sources.identities], Some(&ids))?;
    assert!(
        validate_bindings(
            &document,
            &sources.files[&sources.identities],
            Some(&BTreeSet::new())
        )
        .is_err()
    );
    let mut value = document.into_value();
    value["data"]["models"] = json!([]);
    value["data"]["providers"][0]["status"] = json!("withdrawn");
    let document = Document::from_value(value, &ReaderPolicy::default())?;
    assert!(
        validate_bindings(
            &document,
            &sources.files[&sources.identities],
            Some(&BTreeSet::new())
        )
        .is_err()
    );
    assert!(identity_ids(&json!({"data":[{"id":"org/model"}],"total_count":2})).is_err());
    assert!(
        identity_ids(
            &json!({"data":[{"id":"org/model"}],"links":{"next":"https://example.com/next"}})
        )
        .is_err()
    );
    Ok(())
}
#[test]
fn source_directory_swap_and_crash_recovery_keep_complete_transactions() -> Result<()> {
    let root = fixture()?;
    let original = Sources::load(root.path())?;
    let mut next = Sources::load(root.path())?;
    next.files.get_mut(&next.manifest).context("manifest")?["as_of"] =
        json!("2026-10-22T01:00:00Z");
    next.document()?;
    next.publish(&original, &build_outputs(root.path(), &next)?)?;
    assert_eq!(Sources::load(root.path())?.digest()?, next.digest()?);
    let transaction = root.path().join(".registry-publication");
    fs::create_dir(&transaction)?;
    fs::rename(root.path().join("registry"), transaction.join("previous"))?;
    recover_publication(root.path())?;
    assert_eq!(Sources::load(root.path())?.digest()?, next.digest()?);
    assert!(!transaction.exists());
    Ok(())
}
#[test]
fn numeric_retention_uses_eligible_versions_and_trailing_zero_equivalence() -> Result<()> {
    let root = fixture()?;
    let mut sources = Sources::load(root.path())?;
    let policy = sources
        .manifest
        .parent()
        .context("parent")?
        .join("model-discovery.yaml");
    sources.files.insert(policy,json!({"families":[{"key":"org/model","pattern":"^org/model(?P<version>[0-9]+(?:\\.[0-9]+)*)$","retain_versions":1}],"retired_models":[]}));
    let provider_path = root.path().join("registry/providers/test.yaml");
    let provider = sources.files.get_mut(&provider_path).context("provider")?;
    provider["models"] = json!([{"provider_model_id":"old","canonical_id":"org/model9","status":"active","interfaces":[{"protocol":"messages","operation":"generation"}]},{"provider_model_id":"new","canonical_id":"org/model10","status":"withdrawn","interfaces":[{"protocol":"messages","operation":"generation"}]}]);
    let snapshot = json!({"data":[{"id":"org/model9"},{"id":"org/model10"}]});
    let identities = identity_ids(&snapshot)?;
    sources.files.insert(
        sources.identities.clone(),
        json!([{"canonical_id":"org/model9"}]),
    );
    sources
        .files
        .insert(root.path().join("registry/models/org.yaml"), json!([]));
    discovery::apply(&mut sources, &snapshot, &identities, Utc::now())?;
    assert_eq!(
        sources.files[&root.path().join("registry/models/org.yaml")][0]["id"],
        "org/model9"
    );
    assert_eq!(
        sources.files[&provider_path]["models"][1]["status"],
        "withdrawn"
    );
    Ok(())
}

#[test]
fn late_output_failure_and_concurrent_unrelated_edits_preserve_prior_tree() -> Result<()> {
    let root = fixture()?;
    build(root.path(), false)?;
    let original = Sources::load(root.path())?;
    let prior = fs::read(root.path().join("dist/registry/v1/registry.json"))?;
    let mut next = original.clone();
    next.files.get_mut(&next.manifest).context("manifest")?["as_of"] =
        json!("2026-10-22T01:00:00Z");
    let outputs = build_outputs(root.path(), &next)?;
    assert!(
        next.publish_with(&original, &outputs, |index| {
            if index == 2 {
                bail!("injected late write failure")
            }
            Ok(())
        })
        .is_err()
    );
    assert_eq!(
        Sources::load(root.path())?.tree_digest,
        original.tree_digest
    );
    assert_eq!(
        fs::read(root.path().join("dist/registry/v1/registry.json"))?,
        prior
    );
    assert!(!root.path().join(".registry-publication").exists());
    fs::write(
        root.path().join("registry/README.md"),
        "concurrent user edit",
    )?;
    assert!(next.publish(&original, &outputs).is_err());
    assert_eq!(
        fs::read_to_string(root.path().join("registry/README.md"))?,
        "concurrent user edit"
    );
    assert_eq!(
        fs::read(root.path().join("dist/registry/v1/registry.json"))?,
        prior
    );
    Ok(())
}

#[test]
fn manifest_unknown_data_defaults_and_invalid_policy_are_handled_explicitly() -> Result<()> {
    let root = fixture()?;
    let mut sources = Sources::load(root.path())?;
    sources
        .files
        .get_mut(&sources.manifest)
        .context("manifest")?["data"] = json!({"future_index":{"opaque":true}});
    let path = root.path().join("registry/providers/test.yaml");
    let provider = sources.files.get_mut(&path).context("provider")?;
    provider["defaults"] =
        json!({"interfaces":[{"protocol":"messages","operation":"generation"}],"future":"kept"});
    provider["models"][0]
        .as_object_mut()
        .context("model")?
        .remove("interfaces");
    let document = sources.document()?;
    assert_eq!(document.value()["data"]["future_index"]["opaque"], true);
    assert_eq!(
        document.providers()[0]["models"][0]["interfaces"][0]["protocol"],
        "messages"
    );
    let provider = sources.files.get_mut(&path).context("provider")?;
    provider["models"][0]["interfaces"] = json!([]);
    assert!(sources.document().is_err()); // explicit [] does not inherit
    sources.files.insert(root.path().join("registry/model-discovery.yaml"),json!({"retain_versions":"bad","families":[{"key":"x","pattern":"(?P<version>[0-9]+)","retain_versions":"bad"}]}));
    assert!(
        discovery::validate(&sources.files[&root.path().join("registry/model-discovery.yaml")])
            .is_err()
    );
    for value in [json!(true), json!(42), json!({})] {
        assert!(identity_ids(&json!({"data":[{"id":"org/model"}],"has_more":value})).is_err());
        assert!(
            identity_ids(&json!({"data":[{"id":"org/model"}],"links":{"next":value}})).is_err()
        );
    }
    Ok(())
}

#[test]
fn registered_adapter_discovers_verified_usable_and_curated_new_model() -> Result<()> {
    let root = fixture()?;
    let mut sources = Sources::load(root.path())?;
    let feed = json!({"id":"catalog","kind":"openrouter_models","writes":["models","metadata"],"request":{"url":bitrouter_registry::openrouter::URL,"auth":{"kind":"none"}},"pagination":{"kind":"none"},"inventory":{"mode":"merge","scope":"public","complete":true,"allow_empty":false}});
    let provider = json!({"name":"openrouter","status":"active","billing":"usage","api_base":"https://openrouter.ai/api/v1","auth":{"kind":"none"},"models":[],"auto_sync":{"feeds":[feed]}});
    let snapshot = json!({"data":[{"id":"org/model11","name":"Model 11","architecture":{"input_modalities":["text","image"],"output_modalities":["text"]},"context_length":128000,"supported_parameters":["tools"]},{"id":"org/picture","architecture":{"input_modalities":["text"],"output_modalities":["image"]}}]});
    let ids = identity_ids(&snapshot)?;
    let observed = bitrouter_registry::openrouter::observation(&feed, &snapshot)?;
    let stamp = bitrouter_registry::reconcile::next_stamp(&feed, "test".into(), Utc::now())?;
    let (mut next, _) =
        bitrouter_registry::reconcile::apply(&provider, "catalog", &observed, &stamp)?;
    let mut ledger = json!([]);
    record_openrouter_bindings(&mut ledger, &provider, &observed, &stamp, &ids)?;
    materialize_bindings(&mut next, &ledger, &ids)?;
    assert_eq!(
        next["models"][0]["interfaces"][0]["protocol"],
        "chat_completions"
    );
    assert!(next["models"][0]["interfaces"][0].get("pricing").is_none());
    assert_eq!(next["models"][1]["interfaces"], json!([]));
    sources
        .files
        .remove(&root.path().join("registry/providers/test.yaml"));
    sources
        .files
        .insert(root.path().join("registry/providers/openrouter.yaml"), next);
    sources.files.insert(sources.identities.clone(), ledger);
    sources
        .files
        .insert(root.path().join("registry/models/org.yaml"), json!([]));
    sources.files.insert(root.path().join("registry/model-discovery.yaml"),json!({"families":[{"key":"org/model","pattern":"^org/model(?P<version>[0-9]+)$","retain_versions":1}],"retired_models":[]}));
    discovery::apply(&mut sources, &snapshot, &ids, Utc::now())?;
    let doc = sources.document()?;
    assert_eq!(doc.models()[0]["id"], "org/model11");
    validate_bindings(&doc, &sources.files[&sources.identities], Some(&ids))?;
    let mut generic = observed;
    let pending = hold_unverified_bindings(
        &json!({"name":"other","models":[]}),
        &json!([]),
        &ids,
        &mut generic,
    )?;
    assert_eq!(pending.len(), 2);
    assert!(generic.models[0].value.get("canonical_id").is_none());
    Ok(())
}

#[test]
fn inherited_interface_supports_scoped_pricing_update_without_mutating_defaults() -> Result<()> {
    let price = json!({"id":"prices","kind":"v1_models","writes":["pricing"],"request":{"url":"https://example.com/prices","auth":{"kind":"none"}},"pagination":{"kind":"none"},"response":{"items_path":"/data","mapping":{"/id":"/provider_model_id","/cost":"/interfaces/0/pricing/input_tokens/no_cache"},"defaults":{"interfaces":[{"protocol":"messages","operation":"generation","pricing":{"kind":"token","currency":"USD"}}]}}});
    let mut provider = json!({"name":"test","status":"active","defaults":{"interfaces":[{"protocol":"messages","operation":"generation"}]},"models":[{"provider_model_id":"A","status":"active"}],"auto_sync":{"feeds":[price]}});
    let defaults = provider["defaults"].clone();
    inherit_interfaces(&mut provider)?;
    let observation =
        bitrouter_registry::mapping::normalize(&price, &[json!({"data":[{"id":"A","cost":2}]})])?;
    let stamp = bitrouter_registry::reconcile::next_stamp(&price, "test".into(), Utc::now())?;
    let (next, _) =
        bitrouter_registry::reconcile::apply(&provider, "prices", &observation, &stamp)?;
    assert_eq!(
        next["models"][0]["interfaces"][0]["pricing"]["input_tokens"]["no_cache"],
        2
    );
    assert_eq!(next["defaults"], defaults);
    Ok(())
}

#[test]
fn explicit_unsupported_output_retracts_owned_text_interface() -> Result<()> {
    let feed = json!({"id":"catalog","kind":"openrouter_models","writes":["models","metadata"],"request":{"url":bitrouter_registry::openrouter::URL,"auth":{"kind":"none"}},"pagination":{"kind":"none"},"inventory":{"mode":"merge","scope":"public","complete":true,"allow_empty":false}});
    let provider =
        json!({"name":"openrouter","status":"active","models":[],"auto_sync":{"feeds":[feed]}});
    let initial = json!({"data":[{"id":"org/model","architecture":{"input_modalities":["text"],"output_modalities":["text"]}}]});
    let observation = bitrouter_registry::openrouter::observation(&feed, &initial)?;
    let stamp = bitrouter_registry::reconcile::next_stamp(&feed, "first".into(), Utc::now())?;
    let (one, _) =
        bitrouter_registry::reconcile::apply(&provider, "catalog", &observation, &stamp)?;
    assert_eq!(
        one["models"][0]["interfaces"][0]["protocol"],
        "chat_completions"
    );
    let next = json!({"data":[{"id":"org/model","architecture":{"input_modalities":["text"],"output_modalities":["image"]}}]});
    let observation = bitrouter_registry::openrouter::observation(&feed, &next)?;
    let stamp = bitrouter_registry::reconcile::next_stamp(
        &one["auto_sync"]["feeds"][0],
        "second".into(),
        Utc::now(),
    )?;
    let (two, _) = bitrouter_registry::reconcile::apply(&one, "catalog", &observation, &stamp)?;
    assert_eq!(two["models"][0]["interfaces"], json!([]));
    assert_eq!(two["models"][0]["model_types"], json!([]));
    Ok(())
}

#[test]
fn promotion_ordering_accepts_historical_beta_envelopes_without_executing_them() -> Result<()> {
    let old=br#"{"version":1,"spec_revision":"1.0.0-beta.1","as_of":"2026-10-22T00:00:00Z","obsolete":{"shape":true}}"#;
    assert!(Document::parse(old, &ReaderPolicy::default()).is_err());
    assert_eq!(
        promoted_as_of(old)?.to_rfc3339(),
        "2026-10-22T00:00:00+00:00"
    );
    assert!(
        promoted_as_of(br#"{"version":2,"spec_revision":"2.0.0","as_of":"2026-10-22T00:00:00Z"}"#)
            .is_err()
    );
    Ok(())
}
