use std::{
    collections::HashMap,
    sync::{atomic::AtomicU32, atomic::Ordering, Arc, Mutex},
};

use meta::meta_client::{MetaClientRef, MetaRef};
use models::schema::{make_owner, split_owner, DatabaseSchema};
use parking_lot::RwLock as SyncRwLock;
use snafu::ResultExt;
use tokio::sync::watch::Receiver;
use tokio::sync::RwLock;
use tokio::sync::{mpsc::UnboundedSender, oneshot};
use trace::error;

use crate::{
    compaction::FlushReq,
    context::GlobalSequenceContext,
    database::Database,
    error::MetaSnafu,
    error::Result,
    kv_option::StorageOptions,
    memcache::MemCache,
    summary::{VersionEdit, WriteSummaryRequest},
    tseries_family::{LevelInfo, TseriesFamily, Version},
    Options, TseriesFamilyId,
};

#[derive(Debug)]
pub struct VersionSet {
    opt: Arc<Options>,
    /// Maps DBName -> DB
    dbs: HashMap<String, Arc<RwLock<Database>>>,
}

impl VersionSet {
    pub fn empty(opt: Arc<Options>) -> Self {
        Self {
            opt,
            dbs: HashMap::new(),
        }
    }

    pub async fn new(
        meta: MetaRef,
        opt: Arc<Options>,
        ver_set: HashMap<TseriesFamilyId, Arc<Version>>,
        flush_task_sender: UnboundedSender<FlushReq>,
    ) -> Result<Self> {
        let mut dbs = HashMap::new();
        for (id, ver) in ver_set {
            let owner = ver.database().to_string();
            let (tenant, database) = split_owner(&owner);

            let schema = match meta.tenant_manager().tenant_meta(tenant) {
                None => DatabaseSchema::new(tenant, database),
                Some(client) => match client.get_db_schema(database).context(MetaSnafu)? {
                    None => DatabaseSchema::new(tenant, database),
                    Some(schema) => schema,
                },
            };
            let db: &mut Arc<RwLock<Database>> =
                dbs.entry(owner)
                    .or_insert(Arc::new(RwLock::new(Database::new(
                        schema,
                        opt.clone(),
                        meta.clone(),
                    )?)));

            let tf_id = ver.tf_id();
            db.write()
                .await
                .open_tsfamily(ver, flush_task_sender.clone());
            db.write().await.get_ts_index_or_add(tf_id).await?;
        }

        Ok(Self { dbs, opt })
    }

    pub fn options(&self) -> Arc<Options> {
        self.opt.clone()
    }

    pub fn create_db(
        &mut self,
        schema: DatabaseSchema,
        meta: MetaRef,
    ) -> Result<Arc<RwLock<Database>>> {
        let db = self
            .dbs
            .entry(schema.owner())
            .or_insert(Arc::new(RwLock::new(Database::new(
                schema,
                self.opt.clone(),
                meta.clone(),
            )?)))
            .clone();
        Ok(db)
    }

    pub fn delete_db(&mut self, tenant: &str, database: &str) -> Option<Arc<RwLock<Database>>> {
        let owner = make_owner(tenant, database);
        self.dbs.remove(&owner)
    }

    pub fn db_exists(&self, tenant: &str, database: &str) -> bool {
        let owner = make_owner(tenant, database);
        self.dbs.get(&owner).is_some()
    }

    pub async fn get_db_schema(
        &self,
        tenant: &str,
        database: &str,
    ) -> Result<Option<DatabaseSchema>> {
        let owner = make_owner(tenant, database);
        let db = self.dbs.get(&owner);
        match db {
            None => Ok(None),
            Some(db) => Ok(Some(db.read().await.get_schema()?)),
        }
    }

    pub fn get_all_db(&self) -> &HashMap<String, Arc<RwLock<Database>>> {
        &self.dbs
    }

    pub fn get_db(&self, tenant_name: &str, db_name: &str) -> Option<Arc<RwLock<Database>>> {
        let owner_name = make_owner(tenant_name, db_name);
        if let Some(v) = self.dbs.get(&owner_name) {
            return Some(v.clone());
        }

        None
    }

    pub async fn tsf_num(&self) -> usize {
        let mut size = 0;
        for db in self.dbs.values() {
            size += db.read().await.tsf_num();
        }

        size
    }

    pub async fn get_tsfamily_by_tf_id(
        &self,
        tf_id: u32,
    ) -> Option<Arc<SyncRwLock<TseriesFamily>>> {
        for db in self.dbs.values() {
            if let Some(v) = db.read().await.get_tsfamily(tf_id) {
                return Some(v);
            }
        }

        None
    }

    pub async fn get_tsfamily_by_name_id(
        &self,
        tenant: &str,
        database: &str,
        tf_id: u32,
    ) -> Option<Arc<SyncRwLock<TseriesFamily>>> {
        let owner = make_owner(tenant, database);
        if let Some(db) = self.dbs.get(&owner) {
            return db.read().await.get_tsfamily(tf_id);
        }

        None
    }

    // will delete in cluster version
    pub async fn get_tsfamily_by_name(
        &self,
        tenant: &str,
        database: &str,
    ) -> Option<Arc<SyncRwLock<TseriesFamily>>> {
        let owner = make_owner(tenant, database);
        if let Some(db) = self.dbs.get(&owner) {
            return db.read().await.get_tsfamily_random();
        }

        None
    }

    pub async fn get_version_edits(&self, last_seq: u64) -> Vec<VersionEdit> {
        let mut version_edits = vec![];
        for (name, db) in self.dbs.iter() {
            let mut ves = db.read().await.get_version_edits(last_seq, None);
            version_edits.append(&mut ves);
        }
        version_edits
    }

    /// **Please call this function after system recovered.**
    ///
    /// Get GlobalSequenceContext to store current minimum sequence number of all TseriesFamilies,
    /// one use is fetching wal files which could be deleted.
    pub async fn get_global_sequence_context(&self) -> GlobalSequenceContext {
        let mut min_seq = 0_u64;
        let mut tsf_seq_map: HashMap<TseriesFamilyId, u64> = HashMap::new();
        for (_, database) in self.dbs.iter() {
            for (tsf_id, tsf) in database.read().await.ts_families().iter() {
                let tsf = tsf.read();
                min_seq = min_seq.min(tsf.seq_no());
                tsf_seq_map.insert(*tsf_id, tsf.seq_no());
            }
        }

        GlobalSequenceContext::new(min_seq, tsf_seq_map)
    }
}
