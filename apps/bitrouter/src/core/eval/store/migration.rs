//! Finite offline Legacy graph validation; no evaluator/admission authority.
mod models;
#[cfg(all(test, unix))]
mod tests;
pub(crate) mod transfer;
use super::*;
use crate::offline::evaluation::rows::{self, Row, Tables};
use bitrouter_ai::tenancy::identity::{InstallationId, PrincipalId, TenantId};

pub(crate) struct Graph {
    subjects: Vec<Node<subject_entity::Model>>,
    results: Vec<Node<result_entity::Model>>,
    admissions: Vec<Node<admission_entity::Model>>,
    snapshots: Vec<Node<snapshot_entity::Model>>,
    partitions: BTreeMap<Option<String>, Partition>,
    pub(crate) rows: usize,
    pub(crate) bytes: usize,
}
struct Node<T> {
    row: Row,
    model: T,
    owner: Option<String>,
}
#[derive(serde::Serialize, Clone)]
pub(crate) struct Partition {
    pub(crate) owner: Option<String>,
    pub(crate) table_counts: [u64; 4],
    pub(crate) source_count: u64,
    pub(crate) source_content_digest: String,
}
// This is inert storage assignment data, never Scope/Caller/Grant. Only the
// offline transaction consumer supplies it after checking the target's real
// authority/configuration rows and every M0 mapping against this graph.
pub(crate) struct Assignment {
    pub(crate) tenant: TenantId,
    pub(crate) principal: PrincipalId,
}

impl Graph {
    pub(crate) fn decode(
        input: Tables,
        key: &[u8; 32],
        installation: &InstallationId,
        source_kind: &str,
    ) -> Result<Self> {
        ensure!(
            matches!(source_kind, "local_user" | "shared_user" | "native_owner"),
            "explicit owner namespace required"
        );
        let scope = Scope::legacy();
        let decoder = super::codec::Decoder { scope: &scope };
        let [subjects, results, admissions, snapshots] = input.rows;
        let mut graph = Self {
            subjects: Vec::new(),
            results: Vec::new(),
            admissions: Vec::new(),
            snapshots: Vec::new(),
            partitions: BTreeMap::new(),
            rows: input.row_count,
            bytes: input.cell_bytes,
        };
        let mut subject_keys = BTreeMap::new();
        for row in subjects {
            let model = models::subject(&row)?;
            let value = decoder.decode_subject(&model)?;
            ensure!(
                model.owner_user_id != "*",
                "wildcard subject owner is unresolved"
            );
            ensure!(
                subject_keys
                    .insert(
                        model.eval_id.clone(),
                        (
                            model.owner_user_id.clone(),
                            model.content_digest.clone(),
                            value
                        )
                    )
                    .is_none(),
                "duplicate subject key"
            );
            graph.subjects.push(Node {
                owner: Some(model.owner_user_id.clone()),
                model,
                row,
            });
        }
        let mut result_keys = BTreeMap::new();
        let mut idempotency = BTreeSet::new();
        for row in results {
            let model = models::result(&row)?;
            let value = decoder.decode_result(&model)?;
            let (owner, _, subject) = subject_keys
                .get(&model.eval_id)
                .context("result parent missing")?;
            ensure!(
                owner == &model.owner_user_id,
                "result parent has another owner"
            );
            crate::storage::evaluation::validate_result_for_subject(&value.result, subject)?;
            ensure!(
                idempotency.insert((owner.clone(), model.idempotency_key.clone())),
                "ambiguous idempotency key"
            );
            ensure!(
                result_keys
                    .insert(
                        model.result_id.clone(),
                        (owner.clone(), model.eval_id.clone(), value.content_digest)
                    )
                    .is_none(),
                "duplicate result key"
            );
            graph.results.push(Node {
                owner: Some(owner.clone()),
                model,
                row,
            });
        }
        let mut sequences = BTreeSet::new();
        for row in admissions {
            let model = models::admission(&row)?;
            ensure!(
                model.sequence > 0 && sequences.insert(model.sequence),
                "admission sequence invalid"
            );
            let (owner, _, _) = result_keys
                .get(&model.result_id)
                .context("admission parent missing")?;
            decoder.decode_admission(&model, &model.result_id)?;
            graph.admissions.push(Node {
                owner: Some(owner.clone()),
                model,
                row,
            });
        }
        let mut roots = BTreeSet::new();
        for row in snapshots {
            let model = models::snapshot(&row)?;
            let snapshot = decoder.decode_snapshot(&model)?;
            ensure!(
                roots.insert(model.evidence_root.clone()),
                "duplicate snapshot root"
            );
            ensure!(
                snapshot.entries.len() <= rows::MAX_ROWS,
                "snapshot entry bound"
            );
            let mut entries = BTreeSet::new();
            for entry in &snapshot.entries {
                ensure!(entries.insert(&entry.result_id), "duplicate snapshot entry");
                let (owner, parent, digest) = result_keys
                    .get(&entry.result_id)
                    .context("snapshot result missing")?;
                let (_, subject_digest, _) = subject_keys
                    .get(parent)
                    .context("snapshot subject missing")?;
                ensure!(
                    entry.eval_id == *parent
                        && entry.content_digest == *digest
                        && entry.subject_content_digest == *subject_digest,
                    "snapshot immutable closure mismatch"
                );
                ensure!(
                    model.owner_user_id == "*" || owner == &model.owner_user_id,
                    "snapshot foreign owner"
                );
            }
            // Wildcard roots are not evidence for any one owner, even when their
            // current entries happen to have one owner. Keep their exact row in
            // the one unresolved external-quarantine partition.
            let owner = if model.owner_user_id == "*" {
                None
            } else {
                Some(model.owner_user_id.clone())
            };
            graph.snapshots.push(Node { owner, model, row });
        }
        type PartitionFacts = ([u64; 4], Vec<[u8; 32]>);
        let mut grouped: BTreeMap<Option<String>, PartitionFacts> = BTreeMap::new();
        for (index, nodes) in [
            graph
                .subjects
                .iter()
                .map(|n| (&n.owner, n.row.digest))
                .collect::<Vec<_>>(),
            graph
                .results
                .iter()
                .map(|n| (&n.owner, n.row.digest))
                .collect(),
            graph
                .admissions
                .iter()
                .map(|n| (&n.owner, n.row.digest))
                .collect(),
            graph
                .snapshots
                .iter()
                .map(|n| (&n.owner, n.row.digest))
                .collect(),
        ]
        .into_iter()
        .enumerate()
        {
            for (owner, digest) in nodes {
                let bucket = grouped
                    .entry(owner.clone())
                    .or_insert_with(|| ([0; 4], Vec::new()));
                bucket.0[index] += 1;
                bucket.1.push(digest);
            }
        }
        let mut total = 0_u64;
        for (owner, (counts, mut hashes)) in grouped {
            hashes.sort_unstable();
            let count = hashes.len() as u64;
            total += count;
            let digest = crate::offline::evaluation::partition_digest(
                key,
                installation,
                source_kind,
                owner.as_deref(),
                hashes,
            )?;
            graph.partitions.insert(
                owner.clone(),
                Partition {
                    owner,
                    table_counts: counts,
                    source_count: count,
                    source_content_digest: digest,
                },
            );
        }
        ensure!(total as usize == graph.rows, "partition is not exhaustive");
        Ok(graph)
    }
    pub(crate) fn partitions(&self) -> impl Iterator<Item = &Partition> {
        self.partitions.values()
    }
    /// Check destination closure and indices before DML. No target is authorized
    /// by names or equal hashes, and approved owner merges still cannot collide.
    pub(crate) fn check_keys(
        &self,
        installation: &InstallationId,
        assignments: &BTreeMap<String, Assignment>,
    ) -> Result<()> {
        use crate::storage::scope::physical_key_for_namespace as key;
        let mut subjects = BTreeSet::new();
        let mut results = BTreeSet::new();
        let mut idempotency = BTreeSet::new();
        let mut snapshots = BTreeSet::new();
        let admission_results: BTreeSet<_> = self
            .admissions
            .iter()
            .map(|node| node.model.result_id.as_str())
            .collect();
        for node in &self.subjects {
            if let Some(a) = node.owner.as_ref().and_then(|o| assignments.get(o)) {
                ensure!(
                    subjects.insert(key(
                        installation,
                        &a.tenant,
                        KeyDomain::EvalSubject,
                        &[&node.model.eval_id]
                    )?),
                    "merged subject collision"
                );
            }
        }
        for node in &self.results {
            if let Some(a) = node.owner.as_ref().and_then(|o| assignments.get(o)) {
                // Legacy result insertion can commit before its admission. Keep
                // that valid source partition available for external quarantine,
                // but the ordinary owned reader requires admission for each result.
                ensure!(
                    admission_results.contains(node.model.result_id.as_str()),
                    "mapped result lacks admission history"
                );
                ensure!(
                    results.insert(key(
                        installation,
                        &a.tenant,
                        KeyDomain::EvalResult,
                        &[&node.model.result_id]
                    )?),
                    "merged result collision"
                );
                ensure!(
                    idempotency.insert(key(
                        installation,
                        &a.tenant,
                        KeyDomain::EvalIdempotency,
                        &[&node.model.idempotency_key]
                    )?),
                    "merged idempotency collision"
                );
            }
        }
        for node in &self.snapshots {
            if let Some(a) = node.owner.as_ref().and_then(|o| assignments.get(o)) {
                ensure!(
                    snapshots.insert(key(
                        installation,
                        &a.tenant,
                        KeyDomain::EvalSnapshot,
                        &[&node.model.evidence_root]
                    )?),
                    "merged snapshot collision"
                );
            }
        }
        Ok(())
    }
}
