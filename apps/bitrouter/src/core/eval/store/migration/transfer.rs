//! Conversion to inert SQL cells, after exhaustive mapping and real target checks.
use super::*;
use crate::core::eval::store::imported;
use crate::offline::evaluation::rows::{Cell, Table};
use crate::storage::scope::physical_key_for_namespace as key;

pub(crate) struct Transfer {
    pub tables: [Vec<Vec<Cell>>; 4],
    pub proofs: Vec<Vec<Cell>>,
    pub bytes: usize,
}
pub(crate) struct Anchors<'a> {
    pub source: &'a InstallationId,
    pub kind: &'a str,
    pub mapping: &'a str,
    pub database: &'a str,
    pub ownership: &'a str,
}
impl Graph {
    pub(crate) fn transfer(
        &self,
        target: &InstallationId,
        assignments: &BTreeMap<String, Assignment>,
        anchors: Anchors<'_>,
    ) -> Result<Transfer> {
        self.check_keys(target, assignments)?;
        let mut out = Transfer {
            tables: std::array::from_fn(|_| Vec::new()),
            proofs: Vec::new(),
            bytes: 0,
        };
        for (index, nodes) in [
            self.subjects
                .iter()
                .map(|n| (&n.owner, &n.row))
                .collect::<Vec<_>>(),
            self.results.iter().map(|n| (&n.owner, &n.row)).collect(),
            self.admissions.iter().map(|n| (&n.owner, &n.row)).collect(),
            self.snapshots.iter().map(|n| (&n.owner, &n.row)).collect(),
        ]
        .into_iter()
        .enumerate()
        {
            for (owner, row) in nodes {
                let Some(assignment) = owner.as_ref().and_then(|o| assignments.get(o)) else {
                    continue;
                };
                let mut cells = row.cells.clone();
                cells[0] = Cell::Text(target.to_string());
                cells[1] = Cell::Text(assignment.tenant.to_string());
                cells[2] = Cell::Text(assignment.principal.to_string());
                let physical =
                    |domain, logical: &str| key(target, &assignment.tenant, domain, &[logical]);
                if index != 2 {
                    cells[4] = Cell::Text(key(target, &assignment.tenant, KeyDomain::Owner, &[])?);
                }
                match index {
                    0 => cells[3] = Cell::Text(physical(KeyDomain::EvalSubject, row.text(3)?)?),
                    1 => {
                        cells[3] = Cell::Text(physical(KeyDomain::EvalResult, row.text(3)?)?);
                        cells[5] = Cell::Text(physical(KeyDomain::EvalSubject, row.text(5)?)?);
                        cells[6] = Cell::Text(physical(KeyDomain::EvalIdempotency, row.text(6)?)?);
                    }
                    2 => cells[4] = Cell::Text(physical(KeyDomain::EvalResult, row.text(4)?)?),
                    3 => {
                        let snapshot_key = physical(KeyDomain::EvalSnapshot, row.text(3)?)?;
                        cells[3] = Cell::Text(snapshot_key.clone());
                        let owner = owner.as_ref().context("mapped source owner")?;
                        let proof = imported::Evidence {
                            version: 1,
                            source_installation: anchors.source.clone(),
                            source_kind: anchors.kind.into(),
                            source_owner: owner.clone(),
                            original_root: row.text(3)?.into(),
                            manifest_sha256: imported::manifest_sha(row.text(5)?.as_bytes())
                                .try_into()?,
                            mapping_digest: anchors.mapping.to_owned().try_into()?,
                            database_evidence_digest: anchors.database.to_owned().try_into()?,
                            ownership_evidence_digest: anchors.ownership.to_owned().try_into()?,
                            partition_digest: self
                                .partitions
                                .get(&Some(owner.clone()))
                                .context("partition")?
                                .source_content_digest
                                .clone()
                                .try_into()?,
                            target_installation: target.clone(),
                            target_tenant: assignment.tenant.clone(),
                            target_creator: assignment.principal.clone(),
                            snapshot_key,
                        };
                        proof.validate()?;
                        out.proofs.push(vec![
                            Cell::Text(target.to_string()),
                            Cell::Text(assignment.tenant.to_string()),
                            Cell::Text(assignment.principal.to_string()),
                            Cell::Text(proof.snapshot_key.clone()),
                            Cell::Text(proof.manifest_sha256.as_str().to_owned()),
                            Cell::Text(proof.digest()?),
                            Cell::Text(serde_json::to_string(&proof)?),
                        ]);
                    }
                    _ => anyhow::bail!("unsupported Eval table"),
                }
                out.tables[index].push(cells);
            }
        }
        // Charge transformed cells too; ownership/provenance expansion is real.
        let mut bytes = 0_usize;
        for row in out.tables.iter().flatten().chain(out.proofs.iter()) {
            for cell in row {
                bytes = bytes
                    .checked_add(match cell {
                        Cell::Null => 0,
                        Cell::Integer(_) => 8,
                        Cell::Text(text) => {
                            ensure!(text.len() <= 1024 * 1024, "transformed cell bound");
                            text.len()
                        }
                    })
                    .context("transfer byte overflow")?;
            }
        }
        ensure!(bytes <= rows::MAX_BYTES, "transformed graph byte bound");
        out.bytes = bytes;
        out.check_reader_bounds()?;
        Ok(out)
    }
    pub(crate) fn examples_match(&self, owner: Option<&str>, id: &str, digest: &str) -> bool {
        // Examples identify typed rows, using the same keyed exact-cell digest
        // as partitioning. A duplicate logical ID across tables is ambiguous.
        let hits = self
            .subjects
            .iter()
            .map(|n| (&n.owner, &n.model.eval_id, &n.row))
            .chain(
                self.results
                    .iter()
                    .map(|n| (&n.owner, &n.model.result_id, &n.row)),
            )
            .chain(
                self.snapshots
                    .iter()
                    .map(|n| (&n.owner, &n.model.evidence_root, &n.row)),
            )
            .filter(|(actual, key, _)| actual.as_deref() == owner && key.as_str() == id)
            .collect::<Vec<_>>();
        hits.len() == 1 && hex::encode(hits[0].2.digest) == digest
    }
}

pub(crate) const PROOFS: Table = Table {
    name: "eval_import_provenance",
    fields: &[
        rows::Field {
            name: "installation_id",
            kind: rows::Kind::Text,
        },
        rows::Field {
            name: "tenant_id",
            kind: rows::Kind::Text,
        },
        rows::Field {
            name: "creator_principal_id",
            kind: rows::Kind::Text,
        },
        rows::Field {
            name: "snapshot_key",
            kind: rows::Kind::Text,
        },
        rows::Field {
            name: "manifest_sha256",
            kind: rows::Kind::Text,
        },
        rows::Field {
            name: "provenance_digest",
            kind: rows::Kind::Text,
        },
        rows::Field {
            name: "provenance_json",
            kind: rows::Kind::Text,
        },
    ],
};

impl Transfer {
    fn check_reader_bounds(&self) -> Result<()> {
        // Import cannot commit a manifest that the existing normal owner-bound
        // reader must reject for size. Account repeated parent reads, both exact
        // stored-manifest/proof lookups and the response serialization. This is
        // inert projection sizing, not a Scope or authority constructor.
        let keyed = |rows: &[Vec<Cell>]| -> Result<BTreeMap<String, usize>> {
            rows.iter()
                .map(|r| {
                    let Some(Cell::Text(key)) = r.get(3) else {
                        anyhow::bail!("target physical key");
                    };
                    Ok((key.clone(), projection_bytes(r)?))
                })
                .collect()
        };
        let subjects = keyed(&self.tables[0])?;
        let results = keyed(&self.tables[1])?;
        let proofs = keyed(&self.proofs)?;
        for row in &self.tables[3] {
            let text = |i| -> Result<&str> {
                match row.get(i) {
                    Some(Cell::Text(v)) => Ok(v.as_str()),
                    _ => anyhow::bail!("snapshot projection"),
                }
            };
            let snapshot: EvalSnapshot = serde_json::from_str(text(5)?)?;
            ensure!(
                snapshot.entries.len() <= io::MAX_OWNED_ROWS,
                "import snapshot exceeds owned reader row bound"
            );
            let installation: InstallationId = text(0)?.try_into()?;
            let tenant: TenantId = text(1)?.try_into()?;
            let proof = *proofs.get(text(3)?).context("snapshot proof missing")?;
            ensure!(proof <= 16 * 1024, "import provenance reader bound");
            let json = io::json_size(&snapshot, 8 * 1024 * 1024)?;
            let mut bytes = projection_bytes(row)?
                .checked_add(proof)
                .and_then(|b| b.checked_mul(2))
                .and_then(|b| b.checked_add(json.checked_mul(2)?))
                .context("snapshot reader budget overflow")?;
            for entry in &snapshot.entries {
                for (domain, logical, sizes) in [
                    (KeyDomain::EvalResult, &entry.result_id, &results),
                    (KeyDomain::EvalSubject, &entry.eval_id, &subjects),
                ] {
                    let physical = key(&installation, &tenant, domain, &[logical])?;
                    bytes = bytes
                        .checked_add(*sizes.get(&physical).context("reader parent missing")?)
                        .context("snapshot reader budget overflow")?;
                }
            }
            ensure!(
                bytes <= io::MAX_BYTES,
                "import snapshot exceeds owned reader byte bound"
            );
        }
        Ok(())
    }
}
fn projection_bytes(cells: &[Cell]) -> Result<usize> {
    // io::load_limited counts 64 bytes per projected row plus SQL text-cast
    // widths, including integers. Match that accounting before opening a writer.
    cells.iter().try_fold(64_usize, |total, cell| {
        total
            .checked_add(match cell {
                Cell::Null => 0,
                Cell::Text(text) => text.len(),
                Cell::Integer(value) => value.to_string().len(),
            })
            .context("projection byte overflow")
    })
}
