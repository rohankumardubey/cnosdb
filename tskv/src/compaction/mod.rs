mod compact;
mod flush;
mod picker;

pub use compact::*;
pub use flush::*;
use parking_lot::RwLock;
pub use picker::*;
use std::sync::Arc;

use crate::{
    kv_option::StorageOptions,
    memcache::MemCache,
    summary::VersionEdit,
    tseries_family::{ColumnFile, Version},
    LevelId, TseriesFamilyId,
};

pub struct CompactReq {
    ts_family_id: TseriesFamilyId,
    database: String,
    storage_opt: Arc<StorageOptions>,

    files: Vec<Arc<ColumnFile>>,
    version: Arc<Version>,
    out_level: LevelId,
}

#[derive(Debug)]
pub struct FlushReq {
    pub mems: Vec<(TseriesFamilyId, Arc<RwLock<MemCache>>)>,
}

impl FlushReq {
    pub fn new(mems: Vec<(TseriesFamilyId, Arc<RwLock<MemCache>>)>) -> Self {
        Self { mems }
    }
}
