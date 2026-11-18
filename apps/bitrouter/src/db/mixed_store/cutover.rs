//! A stopped source grants one successor. The byte-exact original lives in the
//! verified rollback image before we replace its old path with a durable fence.
use super::*;
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Read};
use uuid::Uuid;

const MAGIC: &[u8] = b"BITROUTER-M0-SOURCE-RETIRED\n";
const RECEIPT: &str = "cutover.json";

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Fence {
    publication: Uuid,
    destination_hash: String,
    tree_hash: String,
}

fn destination_hash(destination: &Path) -> String {
    hex::encode(Sha256::digest(destination.as_os_str().as_encoded_bytes()))
}

pub(super) fn work_path(parent: &Path, publication: Uuid) -> PathBuf {
    parent.join(format!(".bitrouter-store-split-{publication}"))
}

// Relative names, entry kinds, contents and lengths are bound separately.
// Reject links and special files before opening any candidate.
fn hash_tree(dir: &Path, hash: &mut Sha256) -> Result<()> {
    let mut entries = fs::read_dir(dir)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let filename = entry.file_name();
        let bytes = filename.as_encoded_bytes();
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
        let kind = entry.file_type()?;
        if kind.is_dir() {
            hash.update(b"D");
            hash_tree(&entry.path(), hash)?;
            hash.update(b"E");
        } else {
            ensure!(kind.is_file(), "unexpected cutover entry");
            let mut file = BufReader::new(File::open(entry.path())?);
            hash.update(b"F");
            hash.update(file.get_ref().metadata()?.len().to_be_bytes());
            loop {
                let bytes = file.fill_buf()?;
                if bytes.is_empty() {
                    break;
                }
                let len = bytes.len();
                hash.update(bytes);
                file.consume(len);
            }
        }
    }
    Ok(())
}

fn digest(work: &Path) -> Result<String> {
    ensure!(
        fs::symlink_metadata(work)?.is_dir(),
        "cutover directory missing"
    );
    let mut hash = Sha256::new();
    hash_tree(work, &mut hash)?;
    Ok(hex::encode(hash.finalize()))
}

pub(super) fn fence(
    source: &Path,
    witness: &crate::offline::large::Source,
    work: &Path,
    destination: &Path,
    publication: Uuid,
    stores: &StagedStores,
) -> Result<()> {
    let mut receipt = private_file(&work.join(RECEIPT))?;
    serde_json::to_writer(&mut receipt, stores)?;
    receipt.sync_all()?;
    File::open(work)?.sync_all()?;
    File::open(work.parent().context("cutover parent")?)?.sync_all()?;
    let fence = Fence {
        publication,
        destination_hash: destination_hash(destination),
        tree_hash: digest(work)?,
    };
    let parent = source.parent().context("M0 source parent")?;
    let temporary = parent.join(format!(".bitrouter-source-fence-{publication}"));
    let mut marker = private_file(&temporary)?;
    marker.write_all(MAGIC)?;
    serde_json::to_writer(&mut marker, &fence)?;
    marker.sync_all()?;
    drop(marker);
    witness.check()?;
    fs::rename(&temporary, source)?;
    File::open(parent)?.sync_all().context("source fence durability uncertain; keep both writers stopped and retry the same destination")?;
    Ok(())
}

pub(super) fn resume(source: &Path, destination: &Path) -> Result<Option<StagedStores>> {
    let mut reader = File::open(source)?;
    let mut magic = [0; MAGIC.len()];
    match reader.read_exact(&mut magic) {
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
        Ok(()) if magic != MAGIC => return Ok(None),
        Ok(()) => (),
    }
    ensure!(reader.metadata()?.len() < 1024, "invalid source fence");
    let fence: Fence = serde_json::from_reader(reader)?;
    ensure!(
        destination_hash(destination) == fence.destination_hash,
        "M0 source already retired to a different destination"
    );
    let parent = destination.parent().context("cutover parent")?;
    let work = work_path(parent, fence.publication);
    ensure!(
        digest(&work)? == fence.tree_hash,
        "interrupted cutover contents differ from the approved snapshot"
    );
    let stores: StagedStores = serde_json::from_reader(File::open(work.join(RECEIPT))?)?;
    ensure!(
        stores.dispatch_allowed
            && stores.bitrouter == destination.join(super::GATEWAY_DB)
            && stores.rollback == destination.join("rollback/source.sqlite")
            && stores.bro == destination.join("bro/store.sqlite"),
        "cutover receipt mismatch"
    );
    File::open(source.parent().context("M0 source parent")?)?.sync_all()?;
    publish(&work, destination)?;
    File::open(parent)?.sync_all()?;
    Ok(Some(stores))
}

// The ordinary virtual-key reader operates even on an installation without a
// file-owned tenancy manifest. Review every retained grant before activation.
pub(super) async fn verify_key_grants(reader: &mut sea_orm::sqlx::SqliteConnection) -> Result<()> {
    use futures::TryStreamExt;
    use sea_orm::sqlx::{query_as, query_scalar};
    let invalid: i64 = query_scalar("SELECT EXISTS(SELECT 1 FROM api_keys WHERE typeof(active)<>'integer' OR active NOT IN (0,1) OR typeof(spend_limit_micro_usd) NOT IN ('integer','null') OR typeof(rpm_limit) NOT IN ('integer','null') OR typeof(expires_at) NOT IN ('text','null') OR length(CAST(expires_at AS BLOB))>128 OR user_id IN ('local','anonymous'))").fetch_one(&mut *reader).await?;
    ensure!(
        invalid == 0,
        "invalid ordinary virtual-key authorization record"
    );
    let mut grants = query_as::<_, (i32, Option<String>, Option<i64>, Option<i64>)>(
        "SELECT active,expires_at,spend_limit_micro_usd,rpm_limit FROM api_keys",
    )
    .fetch(reader);
    while let Some((active, expiry, spend, rpm)) = grants.try_next().await? {
        crate::auth::db::key_limits(active, expiry.as_deref(), spend, rpm)?;
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_fenced_source_can_finish_only_its_verified_successor() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let source = root.join("m0.sqlite");
        private_file(&source)?.write_all(b"stopped M0 fixture")?;
        let witness = crate::offline::large::Source::inspect(&source)?;
        let id = Uuid::new_v4();
        let work = work_path(&root, id);
        let destination = root.join("active");
        private_dir(&work)?;
        let stores = StagedStores {
            dispatch_allowed: true,
            rollback: destination.join("rollback/source.sqlite"),
            bitrouter: destination.join(super::super::GATEWAY_DB),
            bro: destination.join("bro/store.sqlite"),
            bitrouter_rows: BTreeMap::new(),
            bro_rows: BTreeMap::new(),
        };
        let proof = work.join("verification");
        private_file(&proof)?.write_all(b"verified")?;
        fence(&source, &witness, &work, &destination, id, &stores)?;
        ensure!(resume(&source, &root.join("second")).is_err());
        fs::write(&proof, b"changed")?;
        ensure!(resume(&source, &destination).is_err() && !destination.exists());
        fs::write(&proof, b"verified")?;
        ensure!(
            resume(&source, &destination)?
                .context("publication")?
                .dispatch_allowed
        );
        ensure!(
            destination.is_dir() && !work.exists() && resume(&source, &root.join("third")).is_err()
        );
        Ok(())
    }
}
