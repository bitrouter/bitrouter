//! Named tests select a backend; each scenario has one shared definition.

mod cases {
    use super::super::{Backend, cases as scenarios};
    use anyhow::Result;

    #[tokio::test]
    async fn immutable_prefixes_keep_old_tool_versions_and_detect_content_corruption() -> Result<()>
    {
        scenarios::immutable_prefixes_keep_old_tool_versions_and_detect_content_corruption(
            Backend::Live,
        )
        .await
    }

    #[tokio::test]
    async fn revision_cas_retries_corrections_and_retractions_preserve_one_label() -> Result<()> {
        scenarios::revision_cas_retries_corrections_and_retractions_preserve_one_label(
            Backend::Live,
        )
        .await
    }

    #[tokio::test]
    async fn invalid_labels_and_out_of_prefix_citations_are_rejected() -> Result<()> {
        scenarios::invalid_labels_and_out_of_prefix_citations_are_rejected(Backend::Live).await
    }

    #[tokio::test]
    async fn late_metering_changes_resources_without_changing_content_or_labels() -> Result<()> {
        scenarios::late_metering_changes_resources_without_changing_content_or_labels(Backend::Live)
            .await
    }

    #[tokio::test]
    async fn inherited_prefixes_family_unions_and_deletion_preserve_native_scope() -> Result<()> {
        scenarios::inherited_prefixes_family_unions_and_deletion_preserve_native_scope(
            Backend::Live,
        )
        .await
    }

    #[tokio::test]
    async fn concurrent_creation_and_revision_selection_are_durable() -> Result<()> {
        scenarios::concurrent_creation_and_revision_selection_are_durable(Backend::Live).await
    }

    #[tokio::test]
    async fn missing_parent_and_capture_gaps_remain_explicit() -> Result<()> {
        scenarios::missing_parent_and_capture_gaps_remain_explicit(Backend::Live).await
    }

    #[tokio::test]
    async fn long_session_manifests_and_large_assessments_round_trip() -> Result<()> {
        scenarios::long_session_manifests_and_large_assessments_round_trip(Backend::Live).await
    }

    #[tokio::test]
    async fn interruption_after_scoring_invalidates_freshness_without_rewriting_the_prefix()
    -> Result<()> {
        scenarios::interruption_after_scoring_invalidates_freshness_without_rewriting_the_prefix(
            Backend::Live,
        )
        .await
    }

    #[tokio::test]
    async fn historical_assessments_lock_both_fork_ancestries_and_keep_duplicate_receipts()
    -> Result<()> {
        scenarios::historical_assessments_lock_both_fork_ancestries_and_keep_duplicate_receipts(
            Backend::Live,
        )
        .await
    }
}

#[cfg(feature = "db-mysql")]
mod mysql_tests {
    use super::super::{Backend, mysql as scenarios};
    use anyhow::Result;

    #[tokio::test]
    #[ignore = "requires db-mysql and BITROUTER_TEST_DATABASE_URL naming a dedicated MySQL database"]
    async fn mysql_freeze_rejects_session_changed_after_snapshot() -> Result<()> {
        scenarios::mysql_freeze_rejects_session_changed_after_snapshot(Backend::Live).await
    }

    #[tokio::test]
    #[ignore = "requires db-mysql and BITROUTER_TEST_DATABASE_URL naming a dedicated MySQL database"]
    async fn mysql_freeze_rejects_session_deleted_after_snapshot() -> Result<()> {
        scenarios::mysql_freeze_rejects_session_deleted_after_snapshot(Backend::Live).await
    }

    #[tokio::test]
    #[ignore = "requires db-mysql and BITROUTER_TEST_DATABASE_URL naming a dedicated MySQL database"]
    async fn mysql_stale_assessment_cannot_overwrite_human_after_snapshot() -> Result<()> {
        scenarios::mysql_stale_assessment_cannot_overwrite_human_after_snapshot(Backend::Live).await
    }
}
