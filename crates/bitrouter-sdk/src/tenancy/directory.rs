use bitrouter_ai::tenancy::{
    directory::{DirectoryCursorV1, DirectoryPageV1, MAX_DIRECTORY_PAGE_BYTES},
    identity::{AuthorityRevision, InstallationId},
};

use super::{TenancyClient, transport};
use crate::error::{Error, Result};

impl TenancyClient {
    /// Read a nonsecret operator directory page using current `control:tenancy` authority.
    /// The first page establishes a revision. Pass it with every subsequent cursor;
    /// on revision conflict discard the partial scan and explicitly start again.
    /// `limit` is an upper bound: encoded bytes or server working capacity may
    /// shorten a page. Follow `next_after`, not the returned row count. Readers
    /// must share the server's grant-scope byte contract for scopes above 256 keys.
    ///
    /// # Errors
    /// Rejects limits outside 1..=100 and a cursor without a revision before HTTP.
    /// Returns authority, revision conflict, bounded transport or response-binding errors.
    pub async fn directory(
        &self,
        installation: &InstallationId,
        authority_revision: Option<AuthorityRevision>,
        after: Option<&DirectoryCursorV1>,
        limit: u16,
    ) -> Result<DirectoryPageV1> {
        if !(1..=100).contains(&limit) || (after.is_some() && authority_revision.is_none()) {
            return Err(Error::Request("invalid directory pagination"));
        }
        let mut url = self.url("tenancy/directory")?;
        url.query_pairs_mut()
            .append_pair("limit", &limit.to_string());
        if let Some(revision) = authority_revision {
            url.query_pairs_mut()
                .append_pair("authority_revision", &revision.get().to_string());
        }
        if let Some(after) = after {
            url.query_pairs_mut().append_pair("after", after.as_str());
        }
        let result: DirectoryPageV1 = self
            .send_bounded(
                self.http
                    .get(url)
                    .header("Bitrouter-Installation-Id", installation.as_str()),
                MAX_DIRECTORY_PAGE_BYTES,
            )
            .await?;
        transport::response_matches(
            result.installation_id == *installation
                && authority_revision.is_none_or(|revision| result.authority_revision == revision)
                && result.entries.len() <= usize::from(limit)
                && after.is_none_or(|cursor| {
                    result
                        .entries
                        .iter()
                        .all(|entry| entry.key() > cursor.key())
                }),
        )?;
        Ok(result)
    }
}
