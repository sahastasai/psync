//! Pattern 1: independent awaits written one after another.
//!
//! Each call below waits on a different request, and none reads another's result, yet each starts
//! only when the previous one has finished. psync puts them in one level of the graph and awaits
//! them together.

use crate::runtime::sleep_ms;
use crate::show::example;

// ---- anza-xyz/agave: `solana validators` ----------------------------------------------------

/// A JSON-RPC client; every call is one round trip to a public RPC node.
pub struct RpcClient;
impl RpcClient {
    pub async fn get_epoch_info(&self) -> Result<u64, String> {
        sleep_ms(40).await;
        Ok(612)
    }
    pub async fn get_vote_accounts(&self) -> Result<Vec<u64>, String> {
        sleep_ms(60).await;
        Ok(vec![3, 1, 2])
    }
    pub async fn get_block_production(&self) -> Result<u64, String> {
        sleep_ms(50).await;
        Ok(431_998)
    }
    pub async fn get_cluster_nodes(&self) -> Result<usize, String> {
        sleep_ms(55).await;
        Ok(1_402)
    }
}

example! {
    origin: "anza-xyz/agave  cli/src/cluster_query.rs  process_show_validators  (chain-17)",
    pattern: "P1 independent awaits",
    summary: "four cluster-wide RPCs, each independent, made one after another",
    options: (),
    call: |f| f(&RpcClient),
    async fn process_show_validators(rpc_client: &RpcClient) -> Result<String, String> {
        let epoch_info = rpc_client.get_epoch_info().await?;
        let vote_accounts = rpc_client.get_vote_accounts().await?;
        let block_production = rpc_client.get_block_production().await?;
        let cluster_nodes = rpc_client.get_cluster_nodes().await?;
        Ok(format!(
            "epoch {epoch_info}: {} vote accounts, {block_production} slots, {cluster_nodes} nodes",
            vote_accounts.len()
        ))
    }
}

// ---- matrix-org/matrix-rust-sdk: `Recovery::disable` ----------------------------------------

/// A homeserver client; account data is read and written with one HTTP request each.
pub struct Client;
impl Client {
    pub async fn get_account_data(&self, _event_type: &str) -> Result<Option<String>, String> {
        sleep_ms(30).await;
        Ok(Some("key-1".into()))
    }
    pub async fn set_account_data(&self, _event_type: &str, _content: &str) -> Result<(), String> {
        sleep_ms(30).await;
        Ok(())
    }
}

example! {
    origin: "matrix-org/matrix-rust-sdk  crates/matrix-sdk/src/encryption/recovery  Recovery::disable  (app-18)",
    pattern: "P1 independent awaits",
    summary: "one GET and eight PUTs to distinct event types; only one PUT needs the GET",
    options: (),
    call: |f| f(&Client),
    async fn disable(client: &Client) -> Result<usize, String> {
        let default_key = client.get_account_data("m.secret_storage.default_key").await?;
        client.set_account_data("m.secret_storage.default_key", "{}").await?;
        if let Some(key_id) = &default_key {
            client.set_account_data(&format!("m.secret_storage.key.{key_id}"), "{}").await?;
        }
        client.set_account_data("m.megolm_backup.v1", "{}").await?;
        client.set_account_data("m.org.matrix.custom.backup_disabled", "{\"disabled\":true}").await?;
        client.set_account_data("m.cross_signing.master", "{}").await?;
        client.set_account_data("m.cross_signing.self_signing", "{}").await?;
        client.set_account_data("m.cross_signing.user_signing", "{}").await?;
        client.set_account_data("m.megolm_backup.v1.key", "{}").await?;
        Ok(8)
    }
}

// ---- loco-rs/loco: `fetch_page` -----------------------------------------------------------------

/// A pooled database connection.
pub struct Db;
impl Db {
    pub async fn count(&self, _table: &str) -> Result<u64, String> {
        sleep_ms(35).await; // COUNT(*) on a large table is often the slower query
        Ok(1_234)
    }
    pub async fn page(&self, _table: &str, offset: u64, limit: u64) -> Result<Vec<u64>, String> {
        sleep_ms(20).await;
        Ok((offset..offset + limit).collect())
    }
}

example! {
    origin: "loco-rs/loco  src/model/query/paginate/mod.rs  fetch_page  (web-29)",
    pattern: "P1 independent awaits",
    summary: "COUNT and the page query, on every paginated endpoint",
    options: (),
    call: |f| f(&Db, 3, 5),
    async fn fetch_page(db: &Db, page: u64, page_size: u64) -> Result<(u64, Vec<u64>), String> {
        let total_items = db.count("users").await?;
        let rows = db.page("users", page * page_size, page_size).await?;
        Ok((total_items.div_ceil(page_size), rows))
    }
}

// ---- LemmyNet/lemmy: `check_notifications_allowed` --------------------------------------------

/// Lemmy's request context; `pool()` hands out a pooled connection.
pub struct LemmyContext;
pub struct DbPool;
impl LemmyContext {
    pub fn pool(&self) -> DbPool {
        DbPool
    }
}

/// A block lookup: `Err` when `blocker` blocks `target`.
pub async fn read_block(_pool: &mut DbPool, blocker: u32, target: u32) -> Result<(), String> {
    sleep_ms(8).await;
    if blocker == target {
        Err(format!("{blocker} blocks {target}"))
    } else {
        Ok(())
    }
}

/// A notification-mode lookup.
pub async fn read_mode(
    _pool: &mut DbPool,
    _person: u32,
    _object: u32,
) -> Result<Option<&'static str>, String> {
    sleep_ms(8).await;
    Ok(Some("all"))
}

example! {
    origin: "LemmyNet/lemmy  crates/api/api_utils/src/notify.rs  check_notifications_allowed  (web-16)",
    pattern: "P1 independent awaits",
    summary: "six independent reads per notification recipient; needs #[psync::fresh] for the pool handle",
    options: (),
    call: |f| f(&LemmyContext, 7, [1, 2, 3, 4]),
    async fn check_notifications_allowed(context: &LemmyContext, recipient: u32, ids: [u32; 4]) -> Result<bool, String> {
        // Every query takes its own pooled connection rather than sharing this one.
        #[psync::fresh]
        let pool = &mut context.pool();
        read_block(pool, recipient, ids[0]).await?;
        read_block(pool, recipient, ids[1]).await?;
        read_block(pool, recipient, ids[2]).await?;
        read_block(pool, recipient, ids[3]).await?;
        let post_mode = read_mode(pool, recipient, ids[0]).await.ok().flatten().unwrap_or("all");
        let community_mode = read_mode(pool, recipient, ids[3]).await.ok().flatten().unwrap_or("all");
        Ok(post_mode != "mute" && community_mode != "mute")
    }
}

pub fn examples() -> Vec<crate::show::Example> {
    vec![
        process_show_validators::example(),
        disable::example(),
        fetch_page::example(),
        check_notifications_allowed::example(),
    ]
}
