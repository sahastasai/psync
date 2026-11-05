//! Pattern 4: a future that could start earlier.
//!
//! An await whose inputs are ready from the start is written after unrelated work, so it starts
//! late. psync starts it as soon as its own inputs exist, next to the chain it was waiting behind.

use crate::runtime::sleep_ms;
use crate::show::example;

// ---- GreptimeTeam/greptimedb: `WriteCache::put_and_upload_sst` ---------------------------------

/// The write cache: builds the SST's index locally and uploads files to object storage.
pub struct WriteCache;
impl WriteCache {
    pub async fn build_index(&self, file_id: u32) -> Result<u64, String> {
        sleep_ms(40).await;
        Ok(u64::from(file_id) * 3)
    }
    pub async fn upload_parquet(&self, file_id: u32) -> Result<u64, String> {
        sleep_ms(50).await;
        Ok(u64::from(file_id) * 1024)
    }
    pub async fn upload_puffin(&self, _file_id: u32, index: u64) -> Result<u64, String> {
        sleep_ms(20).await;
        Ok(index * 8)
    }
}

example! {
    origin: "GreptimeTeam/greptimedb  src/mito2/src/cache/write_cache.rs  put_and_upload_sst  (dbb-19)",
    pattern: "P4 late start",
    summary: "the Parquet upload does not need the index, but starts only after the index is built",
    options: (),
    call: |f| f(&WriteCache, 7),
    async fn put_and_upload_sst(cache: &WriteCache, file_id: u32) -> Result<(u64, u64), String> {
        let index = cache.build_index(file_id).await?;
        let parquet_size = cache.upload_parquet(file_id).await?;
        let puffin_size = cache.upload_puffin(file_id, index).await?;
        Ok((parquet_size, puffin_size))
    }
}

// ---- sigp/lighthouse: checkpoint sync in `ClientBuilder::beacon_chain_builder` -----------------

/// A remote beacon node serving SSZ.
pub struct BeaconNodeHttpClient;
impl BeaconNodeHttpClient {
    pub async fn get_debug_beacon_states_ssz(&self, _id: &str) -> Result<u64, String> {
        sleep_ms(60).await;
        Ok(9_123_456)
    }
    pub async fn get_beacon_blocks_ssz(&self, slot: u64) -> Result<u64, String> {
        sleep_ms(30).await;
        Ok(slot)
    }
    pub async fn get_blobs(&self, block: u64) -> Result<usize, String> {
        sleep_ms(30).await;
        Ok((block % 6) as usize)
    }
}

/// The network's configuration, which knows where to download the genesis state.
pub struct Eth2NetworkConfig;
impl Eth2NetworkConfig {
    pub async fn genesis_state(&self) -> Result<usize, String> {
        sleep_ms(80).await; // tens of MB on mainnet
        Ok(21_063)
    }
}

example! {
    origin: "sigp/lighthouse  beacon_node/client/src/builder.rs  beacon_chain_builder  (chain-15)",
    pattern: "P4 late start",
    summary: "the genesis download needs only the network config, but waits for the state/block/blobs chain",
    options: (),
    call: |f| f(&BeaconNodeHttpClient, &Eth2NetworkConfig),
    async fn checkpoint_sync(remote: &BeaconNodeHttpClient, config: &Eth2NetworkConfig) -> Result<(u64, usize, usize), String> {
        let state = remote.get_debug_beacon_states_ssz("finalized").await?;
        let block = remote.get_beacon_blocks_ssz(state).await?;
        let blobs = remote.get_blobs(block).await?;
        let genesis_state = config.genesis_state().await?;
        Ok((block, blobs, genesis_state))
    }
}

// ---- zed-industries/zed: `fetch_extensions_from_blob_store` ------------------------------------

/// S3 holding extension manifests, and the collab database.
pub struct BlobStore;
pub struct Database;
impl BlobStore {
    pub async fn list_objects(&self, _prefix: &str) -> Result<Vec<String>, String> {
        sleep_ms(30).await;
        Ok((0..4)
            .map(|i| format!("extensions/ext{i}/manifest.json"))
            .collect())
    }
    pub async fn get_manifest(&self, key: &str) -> Result<String, String> {
        sleep_ms(25).await;
        Ok(key.split('/').nth(1).unwrap_or_default().to_string())
    }
}
impl Database {
    pub async fn get_known_extension_versions(&self) -> Result<Vec<String>, String> {
        sleep_ms(45).await;
        Ok(vec!["ext1".into()])
    }
}

example! {
    origin: "zed-industries/zed  crates/collab/src/api/extensions.rs  fetch_extensions_from_blob_store  (app-08)",
    pattern: "P4 late start",
    summary: "the database read needs nothing from S3, but starts after the listing and every manifest",
    options: (bound = 8),
    call: |f| f(&BlobStore, &Database),
    async fn fetch_extensions_from_blob_store(blob_store: &BlobStore, db: &Database) -> Result<Vec<String>, String> {
        let keys = blob_store.list_objects("extensions/").await?;
        let mut manifests = Vec::new();
        for key in keys {
            let manifest = blob_store.get_manifest(&key).await?;
            manifests.push(manifest);
        }
        let known_versions = db.get_known_extension_versions().await?;
        Ok(manifests.into_iter().filter(|m| !known_versions.contains(m)).collect())
    }
}

pub fn examples() -> Vec<crate::show::Example> {
    vec![
        put_and_upload_sst::example(),
        checkpoint_sync::example(),
        fetch_extensions_from_blob_store::example(),
    ]
}
