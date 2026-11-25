//! Pattern 3: per-item chains and staged work.
//!
//! Work that naturally forms one chain per item (list, then fetch each entry) is written as one
//! stage after another, so every item waits for the slowest item of the previous stage. psync
//! lets each item's chain proceed on its own.

use crate::runtime::sleep_ms;
use crate::show::example;
use std::{collections::BTreeMap, sync::LazyLock};

// ---- kube-rs/kube: `Discovery::run` ----------------------------------------------------------

/// The Kubernetes API server client.
pub struct ApiClient;
impl ApiClient {
    pub async fn list_api_groups(&self) -> Result<Vec<String>, String> {
        sleep_ms(20).await;
        Ok((0..10).map(|i| format!("group{i}.example.io")).collect())
    }
    /// `GET /apis/{group}/{version}` for each served version of the group.
    pub async fn query_apis(&self, group: &str) -> Result<Vec<String>, String> {
        sleep_ms(25).await;
        Ok(vec![format!("{group}/v1")])
    }
    pub async fn list_core_api_versions(&self) -> Result<Vec<String>, String> {
        sleep_ms(20).await;
        Ok(vec!["v1".into()])
    }
}

/// Which groups to discover.
pub struct Mode {
    pub skip: &'static str,
}
impl Mode {
    pub fn is_queryable(&self, group: &str) -> bool {
        group != self.skip
    }
}

pub struct Discovery {
    pub client: ApiClient,
    pub mode: Mode,
    pub groups: BTreeMap<String, Vec<String>>,
}

example! {
    origin: "kube-rs/kube  kube-client/src/discovery/mod.rs  Discovery::run  (infra-08)",
    pattern: "P3 per-item chains",
    summary: "list the API groups, then query each queryable group one after another",
    options: (bound = 16),
    call: |f| f(Discovery { client: ApiClient, mode: Mode { skip: "group3.example.io" }, groups: BTreeMap::new() }),
    async fn run(mut discovery: Discovery) -> Result<usize, String> {
        discovery.groups.clear();
        let api_groups = discovery.client.list_api_groups().await?;
        for g in api_groups {
            let key = g.clone();
            if discovery.mode.is_queryable(&key) {
                let apigroup = discovery.client.query_apis(&g).await?;
                discovery.groups.insert(key, apigroup);
            }
        }
        let corekey = String::from("core");
        if discovery.mode.is_queryable(&corekey) {
            let coreapis = discovery.client.list_core_api_versions().await?;
            discovery.groups.insert(corekey, coreapis);
        }
        Ok(discovery.groups.len())
    }
}

// ---- neondatabase/neon: `tenant_scan_remote_handler` -------------------------------------------

/// Remote storage (S3): listings and small object GETs.
pub struct RemoteStorage;
impl RemoteStorage {
    pub async fn list_remote_timelines(&self, shard: u32) -> Result<Vec<u32>, String> {
        sleep_ms(30).await;
        Ok((0..5).map(|t| shard * 10 + t).collect())
    }
    pub async fn download_index_part(&self, _shard: u32, timeline: u32) -> Result<u64, String> {
        sleep_ms(20).await;
        Ok(u64::from(timeline % 7))
    }
    pub async fn download_tenant_manifest(
        &self,
        shard: u32,
        generation: u64,
    ) -> Result<String, String> {
        sleep_ms(20).await;
        Ok(format!("shard {shard} manifest @{generation}"))
    }
}

static SHARDS: LazyLock<Vec<u32>> = LazyLock::new(|| (1..=4).collect());

example! {
    origin: "neondatabase/neon  pageserver/src/http/routes.rs  tenant_scan_remote_handler  (dbc-02)",
    pattern: "P3 per-item chains",
    summary: "per shard: list timelines, fetch every index part, then the manifest; all serial",
    options: (bound = 8),
    call: |f| f(&RemoteStorage, &SHARDS),
    async fn tenant_scan_remote_handler(storage: &RemoteStorage, shards: &Vec<u32>) -> Result<Vec<String>, String> {
        let mut response = Vec::new();
        for shard in shards {
            let timelines = storage.list_remote_timelines(*shard).await?;
            let mut generation = 0;
            for timeline in timelines {
                let index_part = storage.download_index_part(*shard, timeline).await?;
                generation = generation.max(index_part);
            }
            let manifest = storage.download_tenant_manifest(*shard, generation).await?;
            response.push(manifest);
        }
        Ok(response)
    }
}

pub fn examples() -> Vec<crate::show::Example> {
    vec![
        run::example(),
        tenant_scan_remote_handler::example(),
        partition_splits_by_deletes::example(),
    ]
}

// ---- quickwit-oss/quickwit: `DeleteTaskPlanner::partition_splits_by_deletes` --------------------

/// The metastore and the leaf searchers.
pub struct Metastore;
pub struct SearchJobPlacer;
impl Metastore {
    pub async fn list_delete_tasks(&self, opstamp: u64) -> Result<Vec<u64>, String> {
        sleep_ms(15).await;
        Ok(if opstamp % 3 == 0 {
            Vec::new()
        } else {
            vec![opstamp + 1]
        })
    }
}
impl SearchJobPlacer {
    /// Ask a leaf whether the split has documents matching the delete queries.
    pub async fn leaf_search(&self, split: u64, _delete_tasks: &[u64]) -> Result<bool, String> {
        sleep_ms(35).await;
        Ok(split % 2 == 0)
    }
}

static STALE_SPLITS: LazyLock<Vec<u64>> = LazyLock::new(|| (1..=9).collect());

example! {
    origin: "quickwit-oss/quickwit  quickwit-janitor/src/actors/delete_task_planner.rs  partition_splits_by_deletes  (dbc-15)",
    pattern: "P3 per-item chains",
    summary: "per stale split: list its delete tasks, skip it if none, else ask a leaf; one split at a time",
    options: (bound = 16),
    call: |f| f(&Metastore, &SearchJobPlacer, &STALE_SPLITS),
    async fn partition_splits_by_deletes(metastore: &Metastore, placer: &SearchJobPlacer, stale_splits: &Vec<u64>) -> Result<(Vec<u64>, Vec<u64>), String> {
        let mut splits_with_deletes = Vec::new();
        let mut splits_without_deletes = Vec::new();
        for split in stale_splits {
            let delete_tasks = metastore.list_delete_tasks(*split).await?;
            if delete_tasks.is_empty() {
                splits_without_deletes.push(*split);
                continue;
            }
            let has_matches = placer.leaf_search(*split, &delete_tasks).await?;
            if has_matches {
                splits_with_deletes.push(*split);
            } else {
                splits_without_deletes.push(*split);
            }
        }
        Ok((splits_with_deletes, splits_without_deletes))
    }
}
