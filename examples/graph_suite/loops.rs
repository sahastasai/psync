//! Pattern 2: an await inside a loop over independent items.
//!
//! Each iteration waits on its own request; nothing an iteration computes feeds the next one's
//! request. psync starts up to `bound` iterations' requests ahead while the rest of each body
//! still runs in order. When the body first updates state carried between iterations (a dedupe
//! set, a map removal, a permit reservation), psync splits the loop: that part runs for every
//! item first, then the requests run ahead.

use crate::runtime::sleep_ms;
use crate::show::example;
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    sync::LazyLock,
};

/// An object store (S3/GCS/Azure): one round trip per request.
pub struct ObjectStore;
pub enum StoreError {
    NotFound,
    Other(String),
}
impl ObjectStore {
    pub async fn head(&self, path: &str) -> Result<u64, StoreError> {
        sleep_ms(20).await;
        match path {
            p if p.contains("missing") => Err(StoreError::NotFound),
            p if p.contains("denied") => Err(StoreError::Other(format!("access denied: {p}"))),
            p => Ok(p.len() as u64),
        }
    }
    pub async fn get(&self, path: &str) -> Result<String, StoreError> {
        sleep_ms(25).await;
        Ok(format!("schema-of-{path}"))
    }
}

static FILES: LazyLock<Vec<String>> =
    LazyLock::new(|| (0..12).map(|i| format!("part-{i:05}.parquet")).collect());

// ---- delta-io/delta-rs: `check_files_available` ------------------------------------------------

example! {
    origin: "delta-io/delta-rs  crates/core/src/operations/restore.rs  check_files_available  (dbb-01)",
    pattern: "P2 await in a loop",
    summary: "one HEAD per data file during RESTORE; the first missing file fails the restore",
    options: (bound = 8),
    call: |f| f(&ObjectStore, &FILES),
    async fn check_files_available(object_store: &ObjectStore, files: &Vec<String>) -> Result<usize, String> {
        for file in files {
            let file_path = format!("table/{file}");
            match object_store.head(&file_path).await {
                Ok(_) => {}
                Err(StoreError::NotFound) => return Err(format!("missing data file {file}")),
                Err(StoreError::Other(e)) => return Err(e),
            }
        }
        Ok(files.len())
    }
}

// ---- apache/datafusion: Avro `infer_schema` ----------------------------------------------------

pub fn read_avro_schema(bytes: &str) -> Result<String, String> {
    Ok(bytes.trim_start_matches("schema-of-").to_string())
}

example! {
    origin: "apache/datafusion  datafusion/datasource-avro/src/file_format.rs  infer_schema  (dbc-27)",
    pattern: "P2 await in a loop",
    summary: "one GET per Avro file to read its schema, while Parquet's version already runs concurrently",
    options: (bound = 8),
    call: |f| f(&ObjectStore, &FILES),
    async fn infer_schema(store: &ObjectStore, objects: &Vec<String>) -> Result<usize, String> {
        let mut schemas = Vec::with_capacity(objects.len());
        for object in objects {
            let r = match store.get(object).await {
                Ok(r) => r,
                Err(_) => return Err(format!("cannot read {object}")),
            };
            let schema = read_avro_schema(&r)?;
            schemas.push(schema);
        }
        Ok(schemas.len())
    }
}

// ---- LemmyNet/lemmy: `collect_non_local_mentions` ----------------------------------------------

pub struct Person {
    pub inbox: String,
}

/// A WebFinger lookup followed by fetching the actor: an HTTP round trip to a remote instance.
pub async fn webfinger_resolve_actor(identifier: &str) -> Result<Person, String> {
    sleep_ms(45).await;
    if identifier.starts_with("ghost") {
        Err("not found".into())
    } else {
        Ok(Person {
            inbox: format!("https://{identifier}/inbox"),
        })
    }
}

static MENTIONS: LazyLock<Vec<(&str, &str)>> = LazyLock::new(|| {
    vec![
        ("alice", "lemmy.ml"),
        ("bob", "beehaw.org"),
        ("ghost", "gone.example"),
        ("carol", "lemmy.world"),
        ("dave", "sh.itjust.works"),
    ]
});

example! {
    origin: "LemmyNet/lemmy  crates/apub/objects/src/utils/mentions.rs  collect_non_local_mentions  (web-06)",
    pattern: "P2 await in a loop",
    summary: "one WebFinger lookup per @mention when a post or comment is federated",
    options: (bound = 8),
    call: |f| f(&MENTIONS),
    async fn collect_non_local_mentions(mentions: &Vec<(&'static str, &'static str)>) -> Vec<String> {
        let mut addressed_ccs = vec![];
        for (name, domain) in mentions {
            let identifier = format!("{name}@{domain}");
            let person = webfinger_resolve_actor(&identifier).await;
            if let Ok(person) = person {
                addressed_ccs.push(person.inbox);
            }
        }
        addressed_ccs
    }
}

// ---- jdx/mise: `HttpTaskCacheStore::commit` (loop fission) -------------------------------------

/// A content-addressed blob store.
pub struct CasClient;
impl CasClient {
    pub async fn put_blob(&self, _digest: &str) -> Result<(), String> {
        sleep_ms(30).await;
        Ok(())
    }
    pub async fn put_action_result(&self, outputs: usize) -> Result<usize, String> {
        sleep_ms(30).await;
        Ok(outputs)
    }
}

static UPLOADS: LazyLock<Vec<String>> = LazyLock::new(|| {
    ["a1", "b2", "a1", "c3", "d4", "b2", "e5", "f6"]
        .iter()
        .map(|s| s.to_string())
        .collect()
});

example! {
    origin: "jdx/mise  src/task/task_cache/http.rs  HttpTaskCacheStore::commit  (tools-13)",
    pattern: "P2 await in a loop (fission)",
    summary: "deduplicated blob uploads: the dedupe set runs first, then the uploads run ahead",
    options: (bound = 8),
    call: |f| f(&CasClient, &UPLOADS),
    async fn commit(client: &CasClient, uploads: &Vec<String>) -> Result<usize, String> {
        let mut published = BTreeSet::new();
        for upload in uploads {
            if published.insert(upload.clone()) {
                client.put_blob(upload).await?;
            }
        }
        client.put_action_result(published.len()).await
    }
}

// ---- launchbadge/sqlx: `PoolInner::try_min_connections` (while-loop fission, hinted) -----------

/// A connection pool's bookkeeping, reduced to the parts `try_min_connections` touches.
pub struct PoolInner {
    size: Cell<u32>,
    permits: Cell<u32>,
    min_connections: u32,
    idle: RefCell<Vec<u32>>,
}
pub struct Guard(u32);
impl PoolInner {
    pub fn new(min_connections: u32) -> Self {
        Self {
            size: Cell::new(0),
            permits: Cell::new(10),
            min_connections,
            idle: RefCell::default(),
        }
    }
    pub fn size(&self) -> u32 {
        self.size.get()
    }
    pub fn try_acquire(&self) -> Option<u32> {
        let p = self.permits.get();
        (p > 0).then(|| {
            self.permits.set(p - 1);
            p
        })
    }
    pub fn try_increment_size(&self, _permit: u32) -> Result<Guard, ()> {
        self.size.set(self.size.get() + 1);
        Ok(Guard(self.size.get()))
    }
    /// TCP connect, TLS and the database's startup exchange: three round trips.
    pub async fn connect(&self, guard: Guard) -> Result<u32, String> {
        sleep_ms(3 * 15).await;
        Ok(guard.0)
    }
    pub fn release(&self, conn: u32) {
        self.idle.borrow_mut().push(conn);
    }
}

example! {
    origin: "launchbadge/sqlx  sqlx-core/src/pool/inner.rs  try_min_connections  (dba-21)",
    pattern: "P2 await in a loop (while-loop fission, hinted)",
    summary: "reserve each connection in order, then run the handshakes concurrently; needs #[psync::independent]",
    options: (bound = 8),
    call: |f| f(Box::leak(Box::new(PoolInner::new(6)))),
    async fn try_min_connections(pool: &'static PoolInner) -> Result<usize, String> {
        // The condition only depends on the reservations before the await, which tokens cannot show.
        #[psync::independent]
        while pool.size() < pool.min_connections {
            let Some(permit) = pool.try_acquire() else {
                return Ok(pool.idle.borrow().len());
            };
            let Some(guard) = pool.try_increment_size(permit).ok() else {
                return Ok(pool.idle.borrow().len());
            };
            pool.release(pool.connect(guard).await?);
        }
        Ok(pool.idle.borrow().len())
    }
}

// ---- apache/iggy: Iceberg `DynamicRouter::route_data` (loop fission) ---------------------------

/// An Iceberg catalog and object store: writing a batch uploads data files and commits.
pub struct Catalog;
impl Catalog {
    pub async fn write_data(&self, _table: &str, rows: &[u32]) -> Result<usize, String> {
        sleep_ms(40).await;
        Ok(rows.len())
    }
}

static TABLES: LazyLock<Vec<String>> = LazyLock::new(|| {
    ["ns.orders", "ns.users", "ns.events", "ns.audit"]
        .iter()
        .map(|s| s.to_string())
        .collect()
});

pub fn routed() -> BTreeMap<String, Vec<u32>> {
    [
        ("ns.orders", vec![1, 2, 3]),
        ("ns.users", vec![4]),
        ("ns.events", vec![5, 6]),
        ("ns.audit", vec![7, 8, 9, 10]),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

example! {
    origin: "apache/iggy  core/connectors/sinks/iceberg_sink/src/router/dynamic_router.rs  route_data  (infra-22)",
    pattern: "P2 await in a loop (fission)",
    summary: "take each table's batch from the routing map in order, then write the tables concurrently",
    options: (bound = 8),
    call: |f| f(&Catalog, routed(), &TABLES),
    async fn route_data(catalog: &Catalog, mut table_to_message: BTreeMap<String, Vec<u32>>, tables: &Vec<String>) -> Result<usize, String> {
        let mut written = 0;
        for table_name in tables {
            let batch_messages = match table_to_message.remove(table_name) {
                Some(m) => m,
                None => continue,
            };
            let data: Vec<u32> = batch_messages.into_iter().collect();
            catalog.write_data(table_name, &data).await?;
            written += data.len();
        }
        Ok(written)
    }
}

pub fn examples() -> Vec<crate::show::Example> {
    vec![
        check_files_available::example(),
        infer_schema::example(),
        collect_non_local_mentions::example(),
        commit::example(),
        try_min_connections::example(),
        route_data::example(),
    ]
}
