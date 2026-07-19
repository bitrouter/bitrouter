//! Deterministic risk classification of ACP permission requests, shared by
//! the TUI's tiered autonomy and the fleet MCP bridge's auto-policy
//! (TUI_SPEC §5: reversible + in-worktree ⇒ auto; everything else gates).

use agent_client_protocol::schema::v1::{ToolCallUpdateFields, ToolKind};

/// Risk tier of a permission request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    /// Reads/searches and writes confined to the project tree.
    Low,
    /// Deletes, command execution, network access, writes outside the project
    /// tree, or anything unclassifiable (conservative default).
    High,
}

/// Classify from the tool call's structured fields. Conservative: only
/// reads/searches and writes provably confined to the project tree
/// (`workroot`, which also contains `.bitrouter/worktrees/`) classify Low;
/// deletes, command execution, network access, unknown kinds, and
/// unverifiable writes are High. (Spend-based classification needs metering
/// data that isn't available at permission time.)
pub fn classify(fields: &ToolCallUpdateFields, workroot: &std::path::Path) -> Risk {
    match fields.kind {
        Some(ToolKind::Read | ToolKind::Search | ToolKind::Think | ToolKind::SwitchMode) => {
            Risk::Low
        }
        Some(ToolKind::Edit | ToolKind::Move) => {
            let locations = fields.locations.as_deref().unwrap_or(&[]);
            if !locations.is_empty() && locations.iter().all(|l| l.path.starts_with(workroot)) {
                Risk::Low
            } else {
                // Outside the tree, or no locations to verify against.
                Risk::High
            }
        }
        // Delete, Execute (arbitrary commands), Fetch (network), Other/None.
        _ => Risk::High,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::ToolCallLocation;

    fn fields(kind: Option<ToolKind>, paths: &[&str]) -> ToolCallUpdateFields {
        let locations: Vec<ToolCallLocation> =
            paths.iter().map(|p| ToolCallLocation::new(*p)).collect();
        ToolCallUpdateFields::new().kind(kind).locations(locations)
    }

    #[test]
    fn classify_is_deterministic_over_kind_and_paths() {
        let root = std::path::Path::new("/repo");
        // Reads/searches: low regardless of location.
        for kind in [ToolKind::Read, ToolKind::Search, ToolKind::Think] {
            assert_eq!(
                classify(&fields(Some(kind), &["/etc/passwd"]), root),
                Risk::Low,
                "{kind:?} is low"
            );
        }
        // Writes inside the tree (including bitrouter worktrees): low.
        assert_eq!(
            classify(
                &fields(
                    Some(ToolKind::Edit),
                    &["/repo/src/x.rs", "/repo/.bitrouter/worktrees/w1/y.rs"]
                ),
                root
            ),
            Risk::Low
        );
        // Writes outside the tree: high.
        assert_eq!(
            classify(
                &fields(Some(ToolKind::Edit), &["/home/user/.ssh/config"]),
                root
            ),
            Risk::High
        );
        // One outside path taints the whole request.
        assert_eq!(
            classify(
                &fields(Some(ToolKind::Edit), &["/repo/src/x.rs", "/tmp/out"]),
                root
            ),
            Risk::High
        );
        // A write with no locations is unverifiable: high.
        assert_eq!(
            classify(&fields(Some(ToolKind::Edit), &[]), root),
            Risk::High
        );
        // Deletes, execution, network, unknown: high.
        for kind in [
            ToolKind::Delete,
            ToolKind::Execute,
            ToolKind::Fetch,
            ToolKind::Other,
        ] {
            assert_eq!(
                classify(&fields(Some(kind), &["/repo/src/x.rs"]), root),
                Risk::High,
                "{kind:?} is high"
            );
        }
        assert_eq!(
            classify(&fields(None, &["/repo/src/x.rs"]), root),
            Risk::High,
            "missing kind is high"
        );
    }
}
