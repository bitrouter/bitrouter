use super::*;
use serde_json::json;
use std::os::unix::fs::{PermissionsExt, symlink};

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    bundle: PathBuf,
    target: PathBuf,
    plan: PathBuf,
    document: serde_json::Value,
}

impl Fixture {
    fn new() -> Result<Self> {
        let directory = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()?;
        let root = directory.path().canonicalize()?;
        let bundle = root.join("bundle");
        std::fs::create_dir(&bundle)?;
        std::fs::set_permissions(&bundle, std::fs::Permissions::from_mode(0o700))?;
        for name in ["READY", "evidence.json", "inventory.key", "snapshot.sqlite"] {
            files::write_new(&bundle.join(name), name.as_bytes())?;
        }
        let target = root.join("target.sqlite");
        files::write_new(&target, b"not opened during path refusal")?;
        let provisioning = root.join("provisioning.jsonl");
        let approval = root.join("approval.jsonl");
        let producer = root.join("producer.json");
        for path in [&provisioning, &approval, &producer] {
            files::write_new(path, b"{}\n")?;
        }
        let retired = root.join("retirement.jsonl");
        let digest = "0".repeat(64);
        let retirement = format!(
            "{}\n{}\n",
            json!({
                "format": super::super::format::RETIREMENT,
                "source_installation_id": "source", "database_evidence_digest": digest,
                "fence_maintained": true, "target_ingress_fenced": true,
                "selection_exhaustive": true, "unresolved_effects_remain_unknown": true,
                "writer_count": 1, "selected_file_count": 0
            }),
            json!({"kind":"writer", "id":"writer", "process_identity":"process",
            "restart_source":"disabled", "retired":true})
        );
        files::write_new(&retired, retirement.as_bytes())?;
        let anchor = |path: &Path, bytes: &[u8]| json!({"path":path, "sha256":files::sha(bytes)});
        let document = json!({
            "format":super::super::format::PLAN,
            "source": {"bundle":bundle, "installation_id":"source", "evidence_digest":digest,
                "producer":{"commit":"0".repeat(40), "artifact_sha256":digest,
                    "evidence":anchor(&producer, b"{}\n")},
                "tables":[{"table":"tenancy_installation", "rows":1, "value_bytes":1,
                    "digest":digest, "disposition":"retained_archive"}]},
            "target":{"database":target, "installation_id":"target",
                "provisioning":anchor(&provisioning, b"{}\n"),
                "static_manifest":anchor(&approval, b"{}\n"), "wrapping_key_env":"TEST_KEY"},
            "retirement":anchor(&retired, retirement.as_bytes())
        });
        let plan = root.join("plan.json");
        Ok(Self {
            _directory: directory,
            root,
            bundle,
            target,
            plan,
            document,
        })
    }

    fn options(&self) -> large::Options {
        large::Options {
            working_bytes: 4 * 1024 * 1024,
            scratch_directory: self.root.clone(),
            min_free_bytes: 1,
        }
    }

    fn write_plan(&self) -> Result<()> {
        files::write_new(&self.plan, &serde_json::to_vec(&self.document)?)
    }

    fn bundle_unchanged(&self) -> Result<()> {
        assert_eq!(std::fs::read_dir(&self.bundle)?.count(), 4);
        for name in ["READY", "evidence.json", "inventory.key", "snapshot.sqlite"] {
            assert_eq!(std::fs::read(self.bundle.join(name))?, name.as_bytes());
        }
        assert!(!self.root.join(".target.sqlite.tenancy-owner.lock").exists());
        for entry in std::fs::read_dir(&self.root)? {
            assert!(
                !entry?
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".bitrouter-offline-")
            );
        }
        Ok(())
    }
}

#[tokio::test]
async fn scratch_and_output_aliases_are_refused_before_any_input_mutation() -> Result<()> {
    for case in 0..3 {
        let fixture = Fixture::new()?;
        fixture.write_plan()?;
        let mut options = fixture.options();
        let mut output = fixture.root.join("report");
        match case {
            0 => options.scratch_directory = fixture.bundle.clone(),
            1 => output = fixture.bundle.join("report"),
            _ => {
                let alias = fixture.root.join("bundle-alias");
                symlink(&fixture.bundle, &alias)?;
                output = alias.join("report");
            }
        }
        assert!(
            super::super::super::run_with_options(
                &fixture.plan,
                &output,
                ValidationBudget::default(),
                options
            )
            .await
            .is_err()
        );
        fixture.bundle_unchanged()?;
        assert!(!output.exists());
    }
    Ok(())
}

#[tokio::test]
async fn target_inside_bundle_is_refused_before_ownership_lock_creation() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.document["target"]["database"] = json!(fixture.bundle.join("snapshot.sqlite"));
    fixture.write_plan()?;
    assert!(
        run(
            &fixture.plan,
            &fixture.root.join("report"),
            ValidationBudget::default(),
            fixture.options()
        )
        .await
        .is_err()
    );
    fixture.bundle_unchanged()?;
    Ok(())
}

#[tokio::test]
async fn selected_owner_lock_is_refused_before_opening_or_creating_report() -> Result<()> {
    let mut fixture = Fixture::new()?;
    let selected = fixture
        .target
        .with_file_name(".target.sqlite.tenancy-owner.lock");
    files::write_new(&selected, b"immutable selected bytes")?;
    let retired = fixture.root.join("retirement.jsonl");
    let original = std::fs::read_to_string(&retired)?;
    let mut lines = original.lines();
    let mut header: serde_json::Value = serde_json::from_str(lines.next().context("header")?)?;
    header["selected_file_count"] = json!(1);
    let bytes = format!(
        "{header}\n{}\n{}\n",
        lines.next().context("writer")?,
        json!({
            "kind":"file", "path":selected, "writer":"writer", "bytes":24, "keyed_digest":"0".repeat(64)
        })
    );
    std::fs::write(&retired, bytes.as_bytes())?;
    fixture.document["retirement"]["sha256"] = json!(files::sha(bytes.as_bytes()));
    fixture.write_plan()?;
    let output = fixture.root.join("report");
    assert!(
        run(
            &fixture.plan,
            &output,
            ValidationBudget::default(),
            fixture.options()
        )
        .await
        .is_err()
    );
    assert_eq!(std::fs::read(&selected)?, b"immutable selected bytes");
    assert!(!output.exists());
    // The selected lock was never acquired and remains independently lockable.
    files::private_file(&selected, u64::MAX)?.try_lock()?;
    Ok(())
}

#[test]
fn bounded_bootstrap_refuses_dense_decode_without_creating_scratch() -> Result<()> {
    let fixture = Fixture::new()?;
    let path = fixture.root.join("dense.json");
    files::write_new(&path, format!("[{}0]", "0,".repeat(4096)).as_bytes())?;
    let result = inputs::read_control_before_workspace::<Vec<u64>>(&path, 128 * 1024);
    assert!(
        result
            .err()
            .context("dense decode must be refused")?
            .is::<super::super::streams::WorkingCapacity>()
    );
    fixture.bundle_unchanged()?;
    Ok(())
}
