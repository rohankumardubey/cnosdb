use std::ops::Bound;
use std::time::Duration;
use std::{
    borrow::{Borrow, BorrowMut},
    cmp::{self, max, min},
    collections::{HashMap, HashSet},
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
        Arc,
    },
};

use lazy_static::lazy_static;
use parking_lot::{Mutex, RwLock};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch::Receiver;

use config::get_config;
use models::schema::TableColumn;
use models::{ColumnId, FieldId, InMemPoint, SchemaId, SeriesId, Timestamp, ValueType};
use trace::{debug, error, info, warn};
use utils::BloomFilter;

use crate::file_system::file_manager;
use crate::{
    compaction::{CompactReq, FlushReq, LevelCompactionPicker, Picker},
    error::{Error, Result},
    file_utils::{make_delta_file_name, make_tsm_file_name},
    kv_option::{CacheOptions, Options, StorageOptions},
    memcache::{DataType, MemCache},
    summary::{CompactMeta, VersionEdit},
    tsm::{ColumnReader, DataBlock, IndexReader, TsmReader, TsmTombstone},
    ColumnFileId, LevelId, TseriesFamilyId,
};
use crate::{memcache::RowGroup, tsm::BlockMetaIterator};

lazy_static! {
    pub static ref FLUSH_REQ: Arc<Mutex<Vec<FlushReq>>> = Arc::new(Mutex::new(vec![]));
}

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimeRange {
    pub min_ts: i64,
    pub max_ts: i64,
}

impl From<(Bound<i64>, Bound<i64>)> for TimeRange {
    /// TODO 目前TimeRange只支持闭区间
    fn from(range: (Bound<i64>, Bound<i64>)) -> Self {
        let min_ts = match range.0 {
            Bound::Excluded(v) | Bound::Included(v) => v,
            _ => Timestamp::MIN,
        };
        let max_ts = match range.1 {
            Bound::Excluded(v) | Bound::Included(v) => v,
            _ => Timestamp::MAX,
        };

        TimeRange { min_ts, max_ts }
    }
}

impl TimeRange {
    pub fn new(min_ts: i64, max_ts: i64) -> Self {
        Self { min_ts, max_ts }
    }

    pub fn all() -> Self {
        Self {
            min_ts: Timestamp::MIN,
            max_ts: Timestamp::MAX,
        }
    }

    #[inline(always)]
    pub fn overlaps(&self, range: &TimeRange) -> bool {
        !(self.min_ts > range.max_ts || self.max_ts < range.min_ts)
    }

    #[inline(always)]
    pub fn includes(&self, other: &TimeRange) -> bool {
        self.min_ts <= other.min_ts && self.max_ts >= other.max_ts
    }

    #[inline(always)]
    pub fn merge(&mut self, other: &TimeRange) {
        self.min_ts = self.min_ts.min(other.min_ts);
        self.max_ts = self.max_ts.max(other.max_ts);
    }

    #[inline(always)]
    pub fn is_boundless(&self) -> bool {
        self.min_ts == Timestamp::MIN && self.max_ts == Timestamp::MAX
    }

    #[inline(always)]
    pub fn contains(&self, time_stamp: Timestamp) -> bool {
        time_stamp >= self.min_ts && time_stamp <= self.max_ts
    }
}

impl From<(Timestamp, Timestamp)> for TimeRange {
    fn from(time_range: (Timestamp, Timestamp)) -> Self {
        Self {
            min_ts: time_range.0,
            max_ts: time_range.1,
        }
    }
}

impl From<TimeRange> for (Timestamp, Timestamp) {
    fn from(t: TimeRange) -> Self {
        (t.min_ts, t.max_ts)
    }
}

impl PartialOrd for TimeRange {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TimeRange {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        match self.min_ts.cmp(&other.min_ts) {
            cmp::Ordering::Equal => self.max_ts.cmp(&other.max_ts),
            other => other,
        }
    }
}

#[derive(Debug)]
pub struct ColumnFile {
    file_id: ColumnFileId,
    level: LevelId,
    is_delta: bool,
    time_range: TimeRange,
    size: u64,
    field_id_bloom_filter: BloomFilter,
    deleted: AtomicBool,
    compacting: AtomicBool,

    path: PathBuf,
}

impl ColumnFile {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        file_id: ColumnFileId,
        level: LevelId,
        time_range: TimeRange,
        size: u64,
        is_delta: bool,
        path: impl AsRef<Path>,
    ) -> Self {
        Self {
            file_id,
            level,
            is_delta,
            time_range,
            size,
            field_id_bloom_filter: BloomFilter::new(512),
            deleted: AtomicBool::new(false),
            compacting: AtomicBool::new(false),
            path: path.as_ref().into(),
        }
    }

    pub fn with_compact_data(meta: &CompactMeta, path: impl AsRef<Path>) -> Self {
        Self::new(
            meta.file_id,
            meta.level,
            TimeRange::new(meta.min_ts, meta.max_ts),
            meta.file_size,
            meta.is_delta,
            path,
        )
    }

    pub fn file_id(&self) -> ColumnFileId {
        self.file_id
    }

    pub fn level(&self) -> LevelId {
        self.level
    }

    pub fn is_delta(&self) -> bool {
        self.is_delta
    }

    pub fn time_range(&self) -> &TimeRange {
        &self.time_range
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn file_path(&self) -> PathBuf {
        self.path.clone()
    }

    pub fn overlap(&self, time_range: &TimeRange) -> bool {
        self.time_range.overlaps(time_range)
    }

    pub fn contains_field_id(&self, field_id: FieldId) -> bool {
        self.field_id_bloom_filter.contains(&field_id.to_be_bytes())
    }

    pub fn contains_any_field_id(&self, field_ids: &[FieldId]) -> bool {
        for field_id in field_ids {
            if self.field_id_bloom_filter.contains(&field_id.to_be_bytes()) {
                return true;
            }
        }
        false
    }

    pub async fn add_tombstone(&self, field_ids: &[FieldId], time_range: &TimeRange) -> Result<()> {
        let dir = self.path.parent().expect("file has parent");
        // TODO flock tombstone file.
        let mut tombstone = TsmTombstone::open(dir, self.file_id).await?;
        tombstone.add_range(field_ids, time_range).await?;
        tombstone.flush().await?;
        Ok(())
    }
}

impl ColumnFile {
    pub fn is_deleted(&self) -> bool {
        self.deleted.load(Ordering::Acquire)
    }

    pub fn mark_deleted(&self) {
        self.deleted.store(true, Ordering::Release);
    }

    pub fn is_compacting(&self) -> bool {
        self.compacting.load(Ordering::Acquire)
    }

    pub fn mark_compacting(&self) {
        self.compacting.store(true, Ordering::Release);
    }
}

impl Drop for ColumnFile {
    fn drop(&mut self) {
        debug!("Removing file {}", self.file_id);
        if self.is_deleted() {
            let path = self.file_path();
            if let Err(e) = std::fs::remove_file(&path) {
                error!(
                    "Error when removing file {} at '{}': {}",
                    self.file_id,
                    path.display(),
                    e.to_string()
                );
            }
            info!("Removed file {} at '{}", self.file_id, path.display());
        }
    }
}

pub struct FieldFileLocation {
    field_id: u64,
    file: Arc<ColumnFile>,
    reader: TsmReader,
    block_it: BlockMetaIterator,

    read_index: usize,
    data_block: DataBlock,
}

impl FieldFileLocation {
    pub async fn peek(&mut self) -> Result<Option<DataType>, Error> {
        if self.read_index >= self.data_block.len() {
            if let Some(meta) = self.block_it.next() {
                let blk = self.reader.get_data_block(&meta).await?;
                self.read_index = 0;
                self.data_block = blk;
            } else {
                return Ok(None);
            }
        }

        Ok(self.data_block.get(self.read_index))
    }

    pub fn next(&mut self) {
        self.read_index += 1;
    }
}

#[derive(Debug)]
pub struct LevelInfo {
    pub files: Vec<Arc<ColumnFile>>,
    pub database: String,
    pub tsf_id: u32,
    pub storage_opt: Arc<StorageOptions>,
    pub level: u32,
    pub cur_size: u64,
    pub max_size: u64,
    pub time_range: TimeRange,
}

impl LevelInfo {
    pub fn init(
        database: String,
        level: u32,
        tsf_id: u32,
        storage_opt: Arc<StorageOptions>,
    ) -> Self {
        let max_size = storage_opt.level_file_size(level);
        Self {
            files: Vec::new(),
            database,
            tsf_id,
            storage_opt,
            level,
            cur_size: 0,
            max_size,
            time_range: TimeRange {
                min_ts: Timestamp::MAX,
                max_ts: Timestamp::MIN,
            },
        }
    }

    pub fn init_levels(
        database: String,
        tsf_id: u32,
        storage_opt: Arc<StorageOptions>,
    ) -> [LevelInfo; 5] {
        [
            Self::init(database.clone(), 0, tsf_id, storage_opt.clone()),
            Self::init(database.clone(), 1, tsf_id, storage_opt.clone()),
            Self::init(database.clone(), 2, tsf_id, storage_opt.clone()),
            Self::init(database.clone(), 3, tsf_id, storage_opt.clone()),
            Self::init(database, 4, tsf_id, storage_opt),
        ]
    }

    pub fn with_compact_metas(
        database: String,
        vnode_id: TseriesFamilyId,
        storage_opt: Arc<StorageOptions>,
        compact_metas: &[CompactMeta],
    ) -> [LevelInfo; 5] {
        let mut levels = Self::init_levels(database, vnode_id, storage_opt);
        if !compact_metas.is_empty() {
            for c in compact_metas {
                levels[c.level as usize].push_compact_meta(c);
            }
        }
        levels
    }

    pub fn push_compact_meta(&mut self, compact_meta: &CompactMeta) {
        let file_path = if compact_meta.is_delta {
            let base_dir = self.storage_opt.delta_dir(&self.database, self.tsf_id);
            make_delta_file_name(base_dir, compact_meta.file_id)
        } else {
            let base_dir = self.storage_opt.tsm_dir(&self.database, self.tsf_id);
            make_tsm_file_name(base_dir, compact_meta.file_id)
        };
        self.files.push(Arc::new(ColumnFile::with_compact_data(
            compact_meta,
            file_path,
        )));
        self.tsf_id = compact_meta.tsf_id;
        self.cur_size += compact_meta.file_size;
        self.time_range.max_ts = self.time_range.max_ts.max(compact_meta.max_ts);
        self.time_range.min_ts = self.time_range.min_ts.min(compact_meta.min_ts);

        self.sort_file_asc();
    }

    pub fn push_column_file(&mut self, file: Arc<ColumnFile>) {
        self.cur_size += file.size;
        self.time_range.max_ts = self.time_range.max_ts.max(file.time_range.max_ts);
        self.time_range.min_ts = self.time_range.min_ts.min(file.time_range.min_ts);
        self.files.push(file);

        self.sort_file_asc();
    }

    /// Update time_range by a scan with files.
    /// If files is empty, time_range will be (i64::MAX, i64::MIN).
    pub(crate) fn update_time_range(&mut self) {
        let mut min_ts = Timestamp::MAX;
        let mut max_ts = Timestamp::MIN;
        for f in self.files.iter() {
            min_ts = min_ts.min(f.time_range.min_ts);
            max_ts = max_ts.max(f.time_range.max_ts);
        }
        self.time_range = TimeRange::new(min_ts, max_ts);
    }

    pub async fn read_column_file(
        &self,
        tf_id: u32,
        field_id: FieldId,
        time_range: &TimeRange,
    ) -> Vec<DataBlock> {
        let mut data = vec![];
        for file in self.files.iter() {
            if file.is_deleted() || !file.overlap(time_range) {
                continue;
            }

            let tsm_reader = match TsmReader::open(file.file_path()).await {
                Ok(tr) => tr,
                Err(e) => {
                    error!("failed to load tsm reader, in case {:?}", e);
                    return vec![];
                }
            };
            for idx in tsm_reader.index_iterator_opt(field_id) {
                for blk in idx.block_iterator_opt(time_range) {
                    if let Ok(blk) = tsm_reader.get_data_block(&blk).await {
                        data.push(blk);
                    }
                }
            }
        }
        data
    }

    pub fn sort_file_asc(&mut self) {
        self.files
            .sort_by(|a, b| a.file_id.partial_cmp(&b.file_id).unwrap());
    }

    pub fn level(&self) -> u32 {
        self.level
    }
}

#[derive(Debug)]
pub struct Version {
    pub ts_family_id: TseriesFamilyId,
    pub database: String,
    pub storage_opt: Arc<StorageOptions>,
    /// The max seq_no of write batch in wal flushed to column file.
    pub last_seq: u64,
    /// The max timestamp of write batch in wal flushed to column file.
    pub max_level_ts: i64,
    pub levels_info: [LevelInfo; 5],
}

impl Version {
    pub fn new(
        ts_family_id: TseriesFamilyId,
        database: String,
        storage_opt: Arc<StorageOptions>,
        last_seq: u64,
        levels_info: [LevelInfo; 5],
        max_level_ts: i64,
    ) -> Self {
        Self {
            ts_family_id,
            database,
            storage_opt,
            last_seq,
            max_level_ts,
            levels_info,
        }
    }

    /// Creates new Version using current Version and `VersionEdit`s.
    pub fn copy_apply_version_edits(
        &self,
        version_edits: Vec<VersionEdit>,
        last_seq: Option<u64>,
    ) -> Version {
        let mut added_files: HashMap<LevelId, Vec<CompactMeta>> = HashMap::new();
        let mut deleted_files: HashMap<LevelId, HashSet<ColumnFileId>> = HashMap::new();
        for ve in version_edits {
            if !ve.add_files.is_empty() {
                ve.add_files.into_iter().for_each(|f| {
                    added_files.entry(f.level).or_default().push(f);
                });
            }
            if !ve.del_files.is_empty() {
                ve.del_files.into_iter().for_each(|f| {
                    deleted_files
                        .entry(f.level)
                        .or_insert_with(HashSet::new)
                        .insert(f.file_id);
                });
            }
        }

        let mut new_levels = LevelInfo::init_levels(
            self.database.clone(),
            self.ts_family_id,
            self.storage_opt.clone(),
        );
        for level in self.levels_info.iter() {
            for file in level.files.iter() {
                if let Some(true) = deleted_files
                    .get(&file.level)
                    .map(|file_ids| file_ids.contains(&file.file_id))
                {
                    file.mark_deleted();
                    continue;
                }
                new_levels[level.level as usize].push_column_file(file.clone());
            }
            if let Some(files) = added_files.get(&level.level) {
                for file in files.iter() {
                    new_levels[level.level as usize].push_compact_meta(file);
                }
            }
            added_files.remove(&level.level);
            new_levels[level.level as usize].update_time_range();
        }

        let mut new_version = Self {
            ts_family_id: self.ts_family_id,
            database: self.database.clone(),
            storage_opt: self.storage_opt.clone(),
            last_seq: last_seq.unwrap_or(self.last_seq),
            max_level_ts: self.max_level_ts,
            levels_info: new_levels,
        };
        new_version.update_max_level_ts();
        new_version
    }

    fn update_max_level_ts(&mut self) {
        if self.levels_info.is_empty() {
            return;
        }
        let mut max_ts = Timestamp::MIN;
        for level in self.levels_info.iter() {
            if level.files.is_empty() {
                continue;
            }
            for file in level.files.iter() {
                max_ts = file.time_range.max_ts.max(max_ts);
            }
        }

        self.max_level_ts = max_ts;
    }

    pub fn tf_id(&self) -> TseriesFamilyId {
        self.ts_family_id
    }

    pub fn database(&self) -> &str {
        &self.database
    }

    pub fn levels_info(&self) -> &[LevelInfo; 5] {
        &self.levels_info
    }

    pub fn storage_opt(&self) -> Arc<StorageOptions> {
        self.storage_opt.clone()
    }

    pub fn column_files(
        &self,
        field_ids: &[FieldId],
        time_range: &TimeRange,
    ) -> Vec<Arc<ColumnFile>> {
        self.levels_info
            .iter()
            .filter(|level| level.time_range.overlaps(time_range))
            .flat_map(|level| {
                level.files.iter().filter(|f| {
                    f.time_range().overlaps(time_range) && f.contains_any_field_id(field_ids)
                })
            })
            .cloned()
            .collect()
    }

    // todo:
    pub fn get_ts_overlap(&self, level: u32, ts_min: i64, ts_max: i64) -> Vec<Arc<ColumnFile>> {
        vec![]
    }
}

#[derive(Debug)]
pub struct CacheGroup {
    pub mut_cache: Arc<RwLock<MemCache>>,
    pub immut_cache: Vec<Arc<RwLock<MemCache>>>,
}

#[derive(Debug)]
pub struct SuperVersion {
    pub ts_family_id: u32,
    pub storage_opt: Arc<StorageOptions>,
    pub caches: CacheGroup,
    pub version: Arc<Version>,
    pub version_number: u64,
}

impl SuperVersion {
    pub fn new(
        ts_family_id: u32,
        storage_opt: Arc<StorageOptions>,
        caches: CacheGroup,
        version: Arc<Version>,
        version_number: u64,
    ) -> Self {
        Self {
            ts_family_id,
            storage_opt,
            caches,
            version,
            version_number,
        }
    }
}

#[derive(Debug)]
pub struct TseriesFamily {
    tf_id: TseriesFamilyId,
    database: String,
    mut_cache: Arc<RwLock<MemCache>>,
    immut_cache: Vec<Arc<RwLock<MemCache>>>,
    super_version: Arc<SuperVersion>,
    super_version_id: AtomicU64,
    version: Arc<Version>,
    cache_opt: Arc<CacheOptions>,
    storage_opt: Arc<StorageOptions>,
    compact_picker: Arc<dyn Picker>,
    seq_no: u64,
    immut_ts_min: AtomicI64,
    mut_ts_max: AtomicI64,
    flush_task_sender: UnboundedSender<FlushReq>,
}

impl TseriesFamily {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tf_id: TseriesFamilyId,
        database: String,
        cache: MemCache,
        version: Arc<Version>,
        cache_opt: Arc<CacheOptions>,
        storage_opt: Arc<StorageOptions>,
        flush_task_sender: UnboundedSender<FlushReq>,
    ) -> Self {
        let mm = Arc::new(RwLock::new(cache));
        let seq = version.last_seq;
        let max_level_ts = version.max_level_ts;

        Self {
            tf_id,
            database,
            seq_no: seq,
            mut_cache: mm.clone(),
            immut_cache: Default::default(),
            super_version: Arc::new(SuperVersion::new(
                tf_id,
                storage_opt.clone(),
                CacheGroup {
                    mut_cache: mm,
                    immut_cache: Default::default(),
                },
                version.clone(),
                0,
            )),
            super_version_id: AtomicU64::new(0),
            version,
            cache_opt,
            storage_opt,
            compact_picker: Arc::new(LevelCompactionPicker::new()),
            immut_ts_min: AtomicI64::new(max_level_ts),
            mut_ts_max: AtomicI64::new(i64::MIN),
            flush_task_sender,
        }
    }

    pub fn switch_memcache(&mut self, cache: Arc<RwLock<MemCache>>) {
        self.immut_cache.push(self.mut_cache.clone());
        self.new_super_version(self.version.clone());
        self.mut_cache = cache;
    }

    fn new_super_version(&mut self, version: Arc<Version>) {
        self.super_version_id.fetch_add(1, Ordering::SeqCst);
        self.super_version = Arc::new(SuperVersion::new(
            self.tf_id,
            self.storage_opt.clone(),
            CacheGroup {
                mut_cache: self.mut_cache.clone(),
                immut_cache: self.immut_cache.clone(),
            },
            version,
            self.super_version_id.load(Ordering::SeqCst),
        ))
    }

    /// Set new Version into current TsFamily,
    /// then create new SuperVersion, update seq_no
    pub fn new_version(&mut self, new_version: Version) {
        let version = Arc::new(new_version);
        self.new_super_version(version.clone());
        self.seq_no = version.last_seq;
        self.version = version;
    }

    pub fn switch_to_immutable(&mut self) {
        self.immut_cache.push(self.mut_cache.clone());
        self.mut_cache = Arc::from(RwLock::new(MemCache::new(
            self.tf_id,
            self.cache_opt.max_buffer_size,
            self.seq_no,
        )));
        self.new_super_version(self.version.clone());
    }

    /// Check if there are immutable caches to flush.
    ///
    /// If argument `force` is false, immutable caches number should be greater than
    /// configuration `max_immutable_number`.
    ///
    /// If argument force is set to true, then do not check immutable caches number.
    pub(crate) fn flush_req(&mut self, force: bool) -> Option<FlushReq> {
        let len = self.immut_cache.len();
        let mut imut = vec![];
        for mem in self.immut_cache.iter() {
            if !mem.read().flushed {
                imut.push(mem.clone());
            }
        }
        self.immut_cache = imut;

        if len != self.immut_cache.len() {
            self.new_super_version(self.version.clone());
        }

        self.immut_ts_min
            .store(self.mut_ts_max.load(Ordering::Relaxed), Ordering::Relaxed);
        let mut req_mems: Vec<(u32, Arc<RwLock<MemCache>>)> = vec![];
        for mem in self.immut_cache.iter() {
            if mem.read().flushing {
                continue;
            }
            req_mems.push((self.tf_id, mem.clone()));
        }

        if !force && req_mems.len() < self.cache_opt.max_immutable_number as usize {
            return None;
        }

        for mem in req_mems.iter() {
            mem.1.write().flushing = true;
        }

        info!("flush req queue len : {}", req_mems.len());
        Some(FlushReq { mems: req_mems })
    }

    pub(crate) fn wrap_flush_req(&mut self, force: bool) {
        if let Some(req) = self.flush_req(force) {
            self.flush_task_sender
                .send(req)
                .expect("error send flush req to kvcore");
        }
    }

    pub fn put_points(&self, seq: u64, points: HashMap<(SeriesId, SchemaId), RowGroup>) {
        for ((sid, schema_id), group) in points {
            let mem = self.super_version.caches.mut_cache.read();
            mem.write_group(sid, seq, group);
        }
    }

    // pub async fn touch_flush(tsf: &mut TseriesFamily) {
    //     tokio::spawn(|tsf:&mut TseriesFamily| async move {
    //         while tsf.sub_receiver.changed().await.is_ok() {
    //             tsf.check_to_flush()
    //         }
    //     }
    //     );
    // }

    pub fn check_to_flush(&mut self) {
        if self.super_version.caches.mut_cache.read().is_full() {
            info!("mut_cache full,switch to immutable");
            self.switch_to_immutable();
            if self.immut_cache.len() >= self.cache_opt.max_immutable_number as usize {
                self.wrap_flush_req(false);
            }
        }
    }
    pub fn delete_columns(&self, field_ids: &[FieldId]) {
        self.mut_cache.read().delete_columns(field_ids);
        for memcache in self.immut_cache.iter() {
            memcache.read().delete_columns(field_ids);
        }
    }

    pub fn change_column(&self, sids: &[SeriesId], column_name: &str, new_column: &TableColumn) {
        self.mut_cache
            .read()
            .change_column(sids, column_name, new_column);
        for memcache in self.immut_cache.iter() {
            memcache.read().change_column(sids, column_name, new_column);
        }
    }

    pub fn add_column(&self, sids: &[SeriesId], new_column: &TableColumn) {
        self.mut_cache.read().add_column(sids, new_column);
        for memcache in self.immut_cache.iter() {
            memcache.read().add_column(sids, new_column);
        }
    }

    pub fn delete_series(&self, sids: &[SeriesId], time_range: &TimeRange) {
        self.mut_cache.read().delete_series(sids, time_range);
        for memcache in self.immut_cache.iter() {
            memcache.read().delete_series(sids, time_range);
        }
    }

    pub fn pick_compaction(&self) -> Option<CompactReq> {
        self.compact_picker.pick_compaction(self.version.clone())
    }

    pub fn get_version_edit(&self, last_seq: u64, tsf_name: String) -> VersionEdit {
        let mut version_edit = VersionEdit::new_add_vnode(self.tf_id, tsf_name);
        let version = self.version();
        let max_level_ts = version.max_level_ts;
        for files in version.levels_info.iter() {
            for file in files.files.iter() {
                let mut meta = CompactMeta::from(file.as_ref());
                meta.tsf_id = files.tsf_id;
                meta.high_seq = last_seq;
                version_edit.add_file(meta, max_level_ts);
            }
        }

        version_edit
    }

    pub fn tf_id(&self) -> TseriesFamilyId {
        self.tf_id
    }

    pub fn database(&self) -> String {
        self.database.clone()
    }

    pub fn cache(&self) -> &Arc<RwLock<MemCache>> {
        &self.mut_cache
    }

    pub fn im_cache(&self) -> &Vec<Arc<RwLock<MemCache>>> {
        &self.immut_cache
    }

    pub fn super_version(&self) -> Arc<SuperVersion> {
        self.super_version.clone()
    }

    pub fn version(&self) -> Arc<Version> {
        self.version.clone()
    }

    pub fn storage_opt(&self) -> Arc<StorageOptions> {
        self.storage_opt.clone()
    }

    pub fn seq_no(&self) -> u64 {
        self.seq_no
    }
}

#[cfg(test)]
mod test {
    use std::collections::hash_map;
    use std::mem::{size_of, size_of_val};
    use std::{collections::HashMap, path::PathBuf, sync::Arc};

    use parking_lot::{Mutex, RwLock};
    use tokio::sync::mpsc;
    use tokio::sync::mpsc::UnboundedReceiver;

    use models::schema::{DatabaseSchema, TenantOptions};
    use models::{Timestamp, ValueType};
    use trace::info;

    use crate::file_system::file_manager;
    use crate::file_utils::{self, make_tsm_file_name};
    use crate::{
        compaction::{flush_tests::default_with_field_id, run_flush_memtable_job, FlushReq},
        context::GlobalContext,
        kv_option::Options,
        memcache::{FieldVal, MemCache, RowData, RowGroup},
        summary::{CompactMeta, SummaryTask, VersionEdit, WriteSummaryRequest},
        tseries_family::{TimeRange, TseriesFamily, Version},
        tsm::TsmTombstone,
        version_set::VersionSet,
        TseriesFamilyId,
    };
    use config::{get_config, ClusterConfig};
    use meta::meta_client::{MetaRef, RemoteMetaManager};

    use super::{ColumnFile, LevelInfo};

    #[test]
    fn test_version_apply_version_edits_1() {
        //! There is a Version with two levels:
        //! - Lv.0: [ ]
        //! - Lv.1: [ (3, 3001~3000) ]
        //! - Lv.2: [ (1, 1~1000), (2, 1001~2000) ]
        //! - Lv.3: [ ]
        //! - Lv.4: [ ]
        //!
        //! Add (4, 3051~3150) into lv.1, and delete (3, 3001~3000).
        //!
        //! The new Version will like this:
        //! - Lv.0: [ ]
        //! - Lv.1: [ (3, 3051~3150) ]
        //! - Lv.2: [ (1, 1~1000), (2, 1001~2000) ]
        //! - Lv.3: [ ]
        //! - Lv.4: [ ]
        let dir = "/tmp/test/ts_family/1";
        let _ = std::fs::remove_dir(dir);
        std::fs::create_dir_all(dir).unwrap();
        let mut global_config = get_config("../config/config.toml");
        global_config.storage.path = dir.to_string();
        let opt = Arc::new(Options::from(&global_config));

        let database = "test".to_string();
        let ts_family_id = 1;
        let tsm_dir = opt.storage.tsm_dir(&database, ts_family_id);
        #[rustfmt::skip]
            let version = Version {
            ts_family_id,
            database: database.clone(),
            storage_opt: opt.storage.clone(),
            last_seq: 1,
            max_level_ts: 3100,
            levels_info: [
                LevelInfo::init(database.clone(), 0, 0, opt.storage.clone()),
                LevelInfo {
                    files: vec![
                        Arc::new(ColumnFile::new(3, 1, TimeRange::new(3001, 3100), 100, false, make_tsm_file_name(&tsm_dir, 3))),
                    ],
                    database: database.clone(),
                    tsf_id: 1,
                    storage_opt: opt.storage.clone(),
                    level: 1,
                    cur_size: 100,
                    max_size: 1000,
                    time_range: TimeRange::new(3001, 3100),
                },
                LevelInfo {
                    files: vec![
                        Arc::new(ColumnFile::new(1, 2, TimeRange::new(1, 1000), 1000, false, make_tsm_file_name(&tsm_dir, 1))),
                        Arc::new(ColumnFile::new(2, 2, TimeRange::new(1001, 2000), 1000, false, make_tsm_file_name(&tsm_dir, 2))),
                    ],
                    database: database.clone(),
                    tsf_id: 1,
                    storage_opt: opt.storage.clone(),
                    level: 2,
                    cur_size: 2000,
                    max_size: 10000,
                    time_range: TimeRange::new(1, 2000),
                },
                LevelInfo::init(database.clone(), 3, 0, opt.storage.clone()),
                LevelInfo::init(database, 4, 0,opt.storage.clone()),
            ],
        };
        let mut version_edits = Vec::new();
        let mut ve = VersionEdit::new(1);
        #[rustfmt::skip]
        ve.add_file(
            CompactMeta {
                file_id: 4,
                file_size: 100,
                tsf_id: 1,
                level: 1,
                min_ts: 3051,
                max_ts: 3150,
                high_seq: 2,
                low_seq: 2,
                is_delta: false,
            },
            3100,
        );
        version_edits.push(ve);
        let mut ve = VersionEdit::new(1);
        ve.del_file(1, 3, false);
        version_edits.push(ve);
        let new_version = version.copy_apply_version_edits(version_edits, Some(3));

        assert_eq!(new_version.last_seq, 3);
        assert_eq!(new_version.max_level_ts, 3150);

        let lvl = new_version.levels_info.get(1).unwrap();
        assert_eq!(lvl.time_range, TimeRange::new(3051, 3150));
        assert_eq!(lvl.files.len(), 1);
        let col_file = lvl.files.first().unwrap();
        assert_eq!(col_file.time_range, TimeRange::new(3051, 3150));
    }

    #[test]
    fn test_version_apply_version_edits_2() {
        //! There is a Version with two levels:
        //! - Lv.0: [ ]
        //! - Lv.1: [ (3, 3001~3000), (4, 3051~3150) ]
        //! - Lv.2: [ (1, 1~1000), (2, 1001~2000) ]
        //! - Lv.3: [ ]
        //! - Lv.4: [ ]
        //!
        //! 1. Compact [ (3, 3001~3000), (4, 3051~3150) ] into lv.2, and delete them.
        //! 2. Compact [ (1, 1~1000), (2, 1001~2000) ] into lv.3, and delete them.
        //!
        //! The new Version will like this:
        //! - Lv.0: [ ]
        //! - Lv.1: [  ]
        //! - Lv.2: [ (5, 3001~3150) ]
        //! - Lv.3: [ (6, 1~2000) ]
        //! - Lv.4: [ ]
        let dir = "/tmp/test/ts_family/2";
        let _ = std::fs::remove_dir(dir);
        std::fs::create_dir_all(dir).unwrap();
        let mut global_config = get_config("../config/config.toml");
        global_config.storage.path = dir.to_string();
        let opt = Arc::new(Options::from(&global_config));

        let database = "test".to_string();
        let ts_family_id = 1;
        let tsm_dir = opt.storage.tsm_dir(&database, ts_family_id);
        #[rustfmt::skip]
            let version = Version {
            ts_family_id: 1,
            database: database.clone(),
            storage_opt: opt.storage.clone(),
            last_seq: 1,
            max_level_ts: 3150,
            levels_info: [
                LevelInfo::init(database.clone(), 0, 1, opt.storage.clone()),
                LevelInfo {
                    files: vec![
                        Arc::new(ColumnFile::new(3, 1, TimeRange::new(3001, 3100), 100, false, make_tsm_file_name(&tsm_dir, 3))),
                        Arc::new(ColumnFile::new(4, 1, TimeRange::new(3051, 3150), 100, false, make_tsm_file_name(&tsm_dir, 4))),
                    ],
                    database: database.clone(),
                    tsf_id: 1,
                    storage_opt: opt.storage.clone(),
                    level: 1,
                    cur_size: 100,
                    max_size: 1000,
                    time_range: TimeRange::new(3001, 3150),
                },
                LevelInfo {
                    files: vec![
                        Arc::new(ColumnFile::new(1, 2, TimeRange::new(1, 1000), 1000, false, make_tsm_file_name(&tsm_dir, 1))),
                        Arc::new(ColumnFile::new(2, 2, TimeRange::new(1001, 2000), 1000, false, make_tsm_file_name(&tsm_dir, 2))),
                    ],
                    database: database.clone(),
                    tsf_id: 1,
                    storage_opt: opt.storage.clone(),
                    level: 2,
                    cur_size: 2000,
                    max_size: 10000,
                    time_range: TimeRange::new(1, 2000),
                },
                LevelInfo::init(database.clone(), 3, 1,opt.storage.clone()),
                LevelInfo::init(database, 4, 1, opt.storage.clone()),
            ],
        };
        let mut version_edits = Vec::new();
        let mut ve = VersionEdit::new(1);
        #[rustfmt::skip]
        ve.add_file(
            CompactMeta {
                file_id: 5,
                file_size: 150,
                tsf_id: 1,
                level: 2,
                min_ts: 3001,
                max_ts: 3150,
                high_seq: 2,
                low_seq: 2,
                is_delta: false,
            },
            3150,
        );
        #[rustfmt::skip]
        ve.add_file(
            CompactMeta {
                file_id: 6,
                file_size: 2000,
                tsf_id: 1,
                level: 3,
                min_ts: 1,
                max_ts: 2000,
                high_seq: 2,
                low_seq: 2,
                is_delta: false,
            },
            3150,
        );
        version_edits.push(ve);
        let mut ve = VersionEdit::new(1);
        ve.del_file(1, 3, false);
        ve.del_file(1, 4, false);
        ve.del_file(2, 1, false);
        ve.del_file(2, 2, false);
        version_edits.push(ve);
        let new_version = version.copy_apply_version_edits(version_edits, Some(3));

        assert_eq!(new_version.last_seq, 3);
        assert_eq!(new_version.max_level_ts, 3150);

        let lvl = new_version.levels_info.get(1).unwrap();
        assert_eq!(
            lvl.time_range,
            TimeRange::new(Timestamp::MAX, Timestamp::MIN)
        );
        assert_eq!(lvl.files.len(), 0);

        let lvl = new_version.levels_info.get(2).unwrap();
        assert_eq!(lvl.time_range, TimeRange::new(3001, 3150));
        let col_file = lvl.files.last().unwrap();
        assert_eq!(col_file.time_range, TimeRange::new(3001, 3150));

        let lvl = new_version.levels_info.get(3).unwrap();
        assert_eq!(lvl.time_range, TimeRange::new(1, 2000));
        assert_eq!(lvl.files.len(), 1);
        let col_file = lvl.files.last().unwrap();
        assert_eq!(col_file.time_range, TimeRange::new(1, 2000));
    }

    #[tokio::test]
    pub async fn test_tsf_delete() {
        let dir = "/tmp/test/ts_family/tsf_delete";
        let _ = std::fs::remove_dir(dir);
        std::fs::create_dir_all(dir).unwrap();
        let mut global_config = get_config("../config/config.toml");
        global_config.storage.path = dir.to_string();
        let opt = Arc::new(Options::from(&global_config));

        let (flush_task_sender, _) = mpsc::unbounded_channel();
        let database = "db".to_string();
        let tsf = TseriesFamily::new(
            0,
            database.clone(),
            MemCache::new(0, 500, 0),
            Arc::new(Version::new(
                0,
                database.clone(),
                opt.storage.clone(),
                0,
                LevelInfo::init_levels(database, 0, opt.storage.clone()),
                0,
            )),
            opt.cache.clone(),
            opt.storage.clone(),
            flush_task_sender,
        );

        let row_group = RowGroup {
            schema: default_with_field_id(vec![0, 1, 2]),
            range: TimeRange {
                min_ts: 1,
                max_ts: 100,
            },
            rows: vec![RowData {
                ts: 10,
                fields: vec![
                    Some(FieldVal::Integer(11)),
                    Some(FieldVal::Integer(12)),
                    Some(FieldVal::Integer(13)),
                ],
            }],
            size: size_of::<RowGroup>() + 3 * size_of::<u32>() + size_of::<Option<FieldVal>>() + 8,
        };
        let mut points = HashMap::new();
        points.insert((0, 0), row_group);
        tsf.put_points(0, points);

        assert_eq!(
            tsf.mut_cache.read().get_data(0, |_| true, |_| true).len(),
            1
        );
        tsf.delete_series(
            &[0],
            &TimeRange {
                min_ts: 0,
                max_ts: 200,
            },
        );
        assert!(tsf
            .mut_cache
            .read()
            .get_data(0, |_| true, |_| true)
            .is_empty());
    }

    // Util function for testing with summary modification.
    async fn update_ts_family_version(
        version_set: Arc<tokio::sync::RwLock<VersionSet>>,
        ts_family_id: TseriesFamilyId,
        mut summary_task_receiver: UnboundedReceiver<SummaryTask>,
    ) {
        let mut version_edits: Vec<VersionEdit> = Vec::new();
        let mut min_seq: u64 = 0;
        while let Some(summary_task) = summary_task_receiver.recv().await {
            for edit in summary_task.write_summary_request().edits.into_iter() {
                if edit.tsf_id == ts_family_id {
                    version_edits.push(edit.clone());
                    if edit.has_seq_no {
                        min_seq = edit.seq_no;
                    }
                }
            }
        }
        let version_set = version_set.write().await;
        if let Some(ts_family) = version_set.get_tsfamily_by_tf_id(ts_family_id).await {
            let mut ts_family = ts_family.write();
            let new_version = ts_family
                .version()
                .copy_apply_version_edits(version_edits, Some(min_seq));
            ts_family.new_version(new_version);
        }
    }

    #[tokio::test]
    pub async fn test_read_with_tomb() {
        let config = get_config("../config/config_31001.toml");
        let meta_manager: MetaRef = Arc::new(RemoteMetaManager::new(config.cluster));
        let _ = meta_manager
            .tenant_manager()
            .create_tenant("cnosdb".to_string(), TenantOptions::default());
        let dir = PathBuf::from("db/tsm/test/0".to_string());
        if !file_manager::try_exists(&dir) {
            std::fs::create_dir_all(&dir).unwrap();
        }

        let dir = PathBuf::from("data/db".to_string());
        if !file_manager::try_exists(&dir) {
            std::fs::create_dir_all(&dir).unwrap();
        }

        let mem = MemCache::new(0, 1000, 0);
        let row_group = RowGroup {
            schema: default_with_field_id(vec![0, 1, 2]),
            range: TimeRange {
                min_ts: 1,
                max_ts: 100,
            },
            rows: vec![RowData {
                ts: 10,
                fields: vec![
                    Some(FieldVal::Integer(11)),
                    Some(FieldVal::Integer(12)),
                    Some(FieldVal::Integer(13)),
                ],
            }],
            size: size_of::<RowGroup>() + 3 * size_of::<u32>() + size_of::<Option<FieldVal>>() + 8,
        };
        mem.write_group(1, 0, row_group);

        let mem = Arc::new(RwLock::new(mem));
        let req_mem = vec![(0, mem)];
        let flush_seq = FlushReq { mems: req_mem };

        let dir = "/tmp/test/ts_family/read_with_tomb";
        let _ = std::fs::remove_dir(dir);
        std::fs::create_dir_all(dir).unwrap();
        let mut global_config = get_config("../config/config.toml");
        global_config.storage.path = dir.to_string();
        let opt = Arc::new(Options::from(&global_config));

        let database = "test_db".to_string();
        let kernel = Arc::new(GlobalContext::new());
        let (summary_task_sender, summary_task_receiver) = mpsc::unbounded_channel();
        let (compact_task_sender, compact_task_receiver) = mpsc::unbounded_channel();
        let (flush_task_sender, _) = mpsc::unbounded_channel();
        let version_set: Arc<tokio::sync::RwLock<VersionSet>> = Arc::new(tokio::sync::RwLock::new(
            VersionSet::new(
                meta_manager.clone(),
                opt.clone(),
                HashMap::new(),
                flush_task_sender.clone(),
            )
            .await
            .unwrap(),
        ));
        version_set
            .write()
            .await
            .create_db(
                DatabaseSchema::new("cnosdb", &database),
                meta_manager.clone(),
            )
            .unwrap();
        let db = version_set
            .write()
            .await
            .get_db("cnosdb", &database)
            .unwrap();

        let ts_family_id = db
            .write()
            .await
            .add_tsfamily(
                0,
                0,
                None,
                summary_task_sender.clone(),
                flush_task_sender.clone(),
            )
            .read()
            .tf_id();

        run_flush_memtable_job(
            flush_seq,
            kernel,
            version_set.clone(),
            summary_task_sender,
            compact_task_sender,
        )
        .await
        .unwrap();

        update_ts_family_version(version_set.clone(), ts_family_id, summary_task_receiver).await;

        let version_set = version_set.write().await;
        let tsf = version_set
            .get_tsfamily_by_name("cnosdb", &database)
            .await
            .unwrap();
        let version = tsf.write().version();
        version.levels_info[1]
            .read_column_file(
                ts_family_id,
                0,
                &TimeRange {
                    max_ts: 0,
                    min_ts: 0,
                },
            )
            .await;

        let file = version.levels_info[1].files[0].clone();

        let dir = opt.storage.tsm_dir(&database, ts_family_id);
        let mut tombstone = TsmTombstone::open(dir, file.file_id).await.unwrap();
        tombstone
            .add_range(&[0], &TimeRange::new(0, 0))
            .await
            .unwrap();
        tombstone.flush().await.unwrap();

        version.levels_info[1]
            .read_column_file(
                0,
                0,
                &TimeRange {
                    max_ts: 0,
                    min_ts: 0,
                },
            )
            .await;
    }
}
