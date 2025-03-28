use std::{cmp, collections::Bound, mem, pin::Pin, sync::Arc};

use async_lock::{RwLock, RwLockUpgradableReadGuard};
use fusio::DynFs;
use fusio_parquet::writer::AsyncWriter;
use futures_util::StreamExt;
use parquet::arrow::{AsyncArrowWriter, ProjectionMask};
use thiserror::Error;
use tokio::sync::oneshot;

use crate::{
    context::Context,
    fs::{generate_file_id, manager::StoreManager, FileId, FileType},
    inmem::{
        immutable::{ArrowArrays, Builder, Immutable},
        mutable::Mutable,
    },
    ondisk::sstable::SsTable,
    record::{KeyRef, Record, Schema as RecordSchema},
    scope::Scope,
    stream::{level::LevelStream, merge::MergeStream, ScanStream},
    transaction::CommitError,
    version::{edit::VersionEdit, TransactionTs, Version, VersionError, MAX_LEVEL},
    DbOption, DbStorage,
};

#[derive(Debug)]
pub enum CompactTask {
    Freeze,
    Flush(Option<oneshot::Sender<()>>),
}

pub(crate) struct Compactor<R>
where
    R: Record,
{
    pub(crate) option: Arc<DbOption>,
    pub(crate) schema: Arc<RwLock<DbStorage<R>>>,
    pub(crate) ctx: Arc<Context<R>>,
    pub(crate) record_schema: Arc<R::Schema>,
}

impl<R> Compactor<R>
where
    R: Record,
{
    pub(crate) fn new(
        schema: Arc<RwLock<DbStorage<R>>>,
        record_schema: Arc<R::Schema>,
        option: Arc<DbOption>,
        ctx: Arc<Context<R>>,
    ) -> Self {
        Compactor::<R> {
            schema,
            option,
            ctx,
            record_schema,
        }
    }

    pub(crate) async fn check_then_compaction(
        &mut self,
        is_manual: bool,
    ) -> Result<(), CompactionError<R>> {
        let mut guard = self.schema.write().await;

        guard.trigger.reset();

        if !guard.mutable.is_empty() {
            let trigger_clone = guard.trigger.clone();

            let mutable = mem::replace(
                &mut guard.mutable,
                Mutable::new(
                    &self.option,
                    trigger_clone,
                    self.ctx.manager.base_fs(),
                    self.record_schema.clone(),
                )
                .await?,
            );
            let (file_id, immutable) = mutable.into_immutable().await?;
            guard.immutables.push((file_id, immutable));
        } else if !is_manual {
            return Ok(());
        }

        if (is_manual && !guard.immutables.is_empty())
            || guard.immutables.len() > self.option.immutable_chunk_max_num
        {
            let recover_wal_ids = guard.recover_wal_ids.take();
            drop(guard);

            let guard = self.schema.upgradable_read().await;
            let chunk_num = if is_manual {
                guard.immutables.len()
            } else {
                self.option.immutable_chunk_num
            };
            let excess = &guard.immutables[0..chunk_num];

            if let Some(scope) = Self::minor_compaction(
                &self.option,
                recover_wal_ids,
                excess,
                &guard.record_schema,
                &self.ctx.manager,
            )
            .await?
            {
                let version_ref = self.ctx.version_set.current().await;
                let mut version_edits = vec![];
                let mut delete_gens = vec![];

                if self.option.is_threshold_exceeded_major(&version_ref, 0) {
                    Self::major_compaction(
                        &version_ref,
                        &self.option,
                        &scope.min,
                        &scope.max,
                        &mut version_edits,
                        &mut delete_gens,
                        &guard.record_schema,
                        &self.ctx,
                    )
                    .await?;
                }
                version_edits.insert(0, VersionEdit::Add { level: 0, scope });
                version_edits.push(VersionEdit::LatestTimeStamp {
                    ts: version_ref.increase_ts(),
                });

                self.ctx
                    .version_set
                    .apply_edits(version_edits, Some(delete_gens), false)
                    .await?;
            }
            let mut guard = RwLockUpgradableReadGuard::upgrade(guard).await;
            let sources = guard.immutables.split_off(chunk_num);
            let _ = mem::replace(&mut guard.immutables, sources);
        }
        if is_manual {
            self.ctx.version_set.rewrite().await.unwrap();
        }
        Ok(())
    }

    pub(crate) async fn minor_compaction(
        option: &DbOption,
        recover_wal_ids: Option<Vec<FileId>>,
        batches: &[(
            Option<FileId>,
            Immutable<<R::Schema as RecordSchema>::Columns>,
        )],
        schema: &R::Schema,
        manager: &StoreManager,
    ) -> Result<Option<Scope<<R::Schema as RecordSchema>::Key>>, CompactionError<R>> {
        if !batches.is_empty() {
            let level_0_path = option.level_fs_path(0).unwrap_or(&option.base_path);
            let level_0_fs = manager.get_fs(level_0_path);

            let mut min = None;
            let mut max = None;

            let gen = generate_file_id();
            let mut wal_ids = Vec::with_capacity(batches.len());

            let mut writer = AsyncArrowWriter::try_new(
                AsyncWriter::new(
                    level_0_fs
                        .open_options(
                            &option.table_path(gen, 0),
                            FileType::Parquet.open_options(false),
                        )
                        .await?,
                ),
                schema.arrow_schema().clone(),
                Some(option.write_parquet_properties.clone()),
            )?;

            if let Some(mut recover_wal_ids) = recover_wal_ids {
                wal_ids.append(&mut recover_wal_ids);
            }
            for (file_id, batch) in batches {
                if let (Some(batch_min), Some(batch_max)) = batch.scope() {
                    if matches!(min.as_ref().map(|min| min > batch_min), Some(true) | None) {
                        min = Some(batch_min.clone())
                    }
                    if matches!(max.as_ref().map(|max| max < batch_max), Some(true) | None) {
                        max = Some(batch_max.clone())
                    }
                }
                writer.write(batch.as_record_batch()).await?;
                if let Some(file_id) = file_id {
                    wal_ids.push(*file_id);
                }
            }
            writer.close().await?;
            return Ok(Some(Scope {
                min: min.ok_or(CompactionError::EmptyLevel)?,
                max: max.ok_or(CompactionError::EmptyLevel)?,
                gen,
                wal_ids: Some(wal_ids),
            }));
        }
        Ok(None)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn major_compaction(
        version: &Version<R>,
        option: &DbOption,
        mut min: &<R::Schema as RecordSchema>::Key,
        mut max: &<R::Schema as RecordSchema>::Key,
        version_edits: &mut Vec<VersionEdit<<R::Schema as RecordSchema>::Key>>,
        delete_gens: &mut Vec<(FileId, usize)>,
        instance: &R::Schema,
        ctx: &Context<R>,
    ) -> Result<(), CompactionError<R>> {
        let mut level = 0;

        while level < MAX_LEVEL - 2 {
            if !option.is_threshold_exceeded_major(version, level) {
                break;
            }
            let (meet_scopes_l, start_l, end_l) = Self::this_level_scopes(version, min, max, level);
            let (meet_scopes_ll, start_ll, end_ll) =
                Self::next_level_scopes(version, &mut min, &mut max, level, &meet_scopes_l)?;

            let level_path = option.level_fs_path(level).unwrap_or(&option.base_path);
            let level_fs = ctx.manager.get_fs(level_path);
            let mut streams = Vec::with_capacity(meet_scopes_l.len() + meet_scopes_ll.len());
            // This Level
            if level == 0 {
                for scope in meet_scopes_l.iter() {
                    let file = level_fs
                        .open_options(
                            &option.table_path(scope.gen, level),
                            FileType::Parquet.open_options(true),
                        )
                        .await?;

                    streams.push(ScanStream::SsTable {
                        inner: SsTable::open(ctx.parquet_lru.clone(), scope.gen, file)
                            .await?
                            .scan(
                                (Bound::Unbounded, Bound::Unbounded),
                                u32::MAX.into(),
                                None,
                                ProjectionMask::all(),
                            )
                            .await?,
                    });
                }
            } else {
                let (lower, upper) = Self::full_scope(&meet_scopes_l)?;
                let level_scan_l = LevelStream::new(
                    version,
                    level,
                    start_l,
                    end_l,
                    (Bound::Included(lower), Bound::Included(upper)),
                    u32::MAX.into(),
                    None,
                    ProjectionMask::all(),
                    level_fs.clone(),
                    ctx.parquet_lru.clone(),
                )
                .ok_or(CompactionError::EmptyLevel)?;

                streams.push(ScanStream::Level {
                    inner: level_scan_l,
                });
            }
            if !meet_scopes_ll.is_empty() {
                // Next Level
                let (lower, upper) = Self::full_scope(&meet_scopes_ll)?;
                let level_scan_ll = LevelStream::new(
                    version,
                    level + 1,
                    start_ll,
                    end_ll,
                    (Bound::Included(lower), Bound::Included(upper)),
                    u32::MAX.into(),
                    None,
                    ProjectionMask::all(),
                    level_fs.clone(),
                    ctx.parquet_lru.clone(),
                )
                .ok_or(CompactionError::EmptyLevel)?;

                streams.push(ScanStream::Level {
                    inner: level_scan_ll,
                });
            }

            let level_l_path = option.level_fs_path(level + 1).unwrap_or(&option.base_path);
            let level_l_fs = ctx.manager.get_fs(level_l_path);
            Self::build_tables(
                option,
                version_edits,
                level + 1,
                streams,
                instance,
                level_l_fs,
            )
            .await?;

            for scope in meet_scopes_l {
                version_edits.push(VersionEdit::Remove {
                    level: level as u8,
                    gen: scope.gen,
                });
                delete_gens.push((scope.gen, level));
            }
            for scope in meet_scopes_ll {
                version_edits.push(VersionEdit::Remove {
                    level: (level + 1) as u8,
                    gen: scope.gen,
                });
                delete_gens.push((scope.gen, level + 1));
            }
            level += 1;
        }

        Ok(())
    }

    fn next_level_scopes<'a>(
        version: &'a Version<R>,
        min: &mut &'a <R::Schema as RecordSchema>::Key,
        max: &mut &'a <R::Schema as RecordSchema>::Key,
        level: usize,
        meet_scopes_l: &[&'a Scope<<R::Schema as RecordSchema>::Key>],
    ) -> Result<
        (
            Vec<&'a Scope<<R::Schema as RecordSchema>::Key>>,
            usize,
            usize,
        ),
        CompactionError<R>,
    > {
        let mut meet_scopes_ll = Vec::new();
        let mut start_ll = 0;
        let mut end_ll = 0;

        if !version.level_slice[level + 1].is_empty() {
            *min = meet_scopes_l
                .iter()
                .map(|scope| &scope.min)
                .min()
                .ok_or(CompactionError::EmptyLevel)?;

            *max = meet_scopes_l
                .iter()
                .map(|scope| &scope.max)
                .max()
                .ok_or(CompactionError::EmptyLevel)?;

            start_ll = Version::<R>::scope_search(min, &version.level_slice[level + 1]);
            end_ll = Version::<R>::scope_search(max, &version.level_slice[level + 1]);

            let next_level_len = version.level_slice[level + 1].len();
            for scope in version.level_slice[level + 1]
                [start_ll..cmp::min(end_ll + 1, next_level_len)]
                .iter()
            {
                if scope.contains(min) || scope.contains(max) {
                    meet_scopes_ll.push(scope);
                }
            }
        }
        Ok((meet_scopes_ll, start_ll, end_ll))
    }

    fn this_level_scopes<'a>(
        version: &'a Version<R>,
        min: &<R::Schema as RecordSchema>::Key,
        max: &<R::Schema as RecordSchema>::Key,
        level: usize,
    ) -> (
        Vec<&'a Scope<<R::Schema as RecordSchema>::Key>>,
        usize,
        usize,
    ) {
        let mut meet_scopes_l = Vec::new();
        let mut start_l = Version::<R>::scope_search(min, &version.level_slice[level]);
        let mut end_l = start_l;
        let option = version.option();

        for scope in version.level_slice[level][start_l..].iter() {
            if (scope.contains(min) || scope.contains(max))
                && meet_scopes_l.len() <= option.major_l_selection_table_max_num
            {
                meet_scopes_l.push(scope);
                end_l += 1;
            } else {
                break;
            }
        }
        if meet_scopes_l.is_empty() {
            start_l = 0;
            end_l = cmp::min(
                option.major_default_oldest_table_num,
                version.level_slice[level].len(),
            );

            for scope in version.level_slice[level][..end_l].iter() {
                if meet_scopes_l.len() > option.major_l_selection_table_max_num {
                    break;
                }
                meet_scopes_l.push(scope);
            }
        }
        (meet_scopes_l, start_l, end_l - 1)
    }

    async fn build_tables<'scan>(
        option: &DbOption,
        version_edits: &mut Vec<VersionEdit<<R::Schema as RecordSchema>::Key>>,
        level: usize,
        streams: Vec<ScanStream<'scan, R>>,
        schema: &R::Schema,
        fs: &Arc<dyn DynFs>,
    ) -> Result<(), CompactionError<R>> {
        let mut stream = MergeStream::<R>::from_vec(streams, u32::MAX.into()).await?;

        // Kould: is the capacity parameter necessary?
        let mut builder =
            <R::Schema as RecordSchema>::Columns::builder(schema.arrow_schema().clone(), 8192);
        let mut min = None;
        let mut max = None;

        while let Some(result) = Pin::new(&mut stream).next().await {
            let entry = result?;
            let key = entry.key();

            if min.is_none() {
                min = Some(key.value.clone().to_key())
            }
            max = Some(key.value.clone().to_key());
            builder.push(key, entry.value());

            if builder.written_size() >= option.max_sst_file_size {
                Self::build_table(
                    option,
                    version_edits,
                    level,
                    &mut builder,
                    &mut min,
                    &mut max,
                    schema,
                    fs,
                )
                .await?;
            }
        }
        if builder.written_size() > 0 {
            Self::build_table(
                option,
                version_edits,
                level,
                &mut builder,
                &mut min,
                &mut max,
                schema,
                fs,
            )
            .await?;
        }
        Ok(())
    }

    fn full_scope<'a>(
        meet_scopes: &[&'a Scope<<R::Schema as RecordSchema>::Key>],
    ) -> Result<
        (
            &'a <R::Schema as RecordSchema>::Key,
            &'a <R::Schema as RecordSchema>::Key,
        ),
        CompactionError<R>,
    > {
        let lower = &meet_scopes.first().ok_or(CompactionError::EmptyLevel)?.min;
        let upper = &meet_scopes.last().ok_or(CompactionError::EmptyLevel)?.max;
        Ok((lower, upper))
    }

    #[allow(clippy::too_many_arguments)]
    async fn build_table(
        option: &DbOption,
        version_edits: &mut Vec<VersionEdit<<R::Schema as RecordSchema>::Key>>,
        level: usize,
        builder: &mut <<R::Schema as RecordSchema>::Columns as ArrowArrays>::Builder,
        min: &mut Option<<R::Schema as RecordSchema>::Key>,
        max: &mut Option<<R::Schema as RecordSchema>::Key>,
        schema: &R::Schema,
        fs: &Arc<dyn DynFs>,
    ) -> Result<(), CompactionError<R>> {
        debug_assert!(min.is_some());
        debug_assert!(max.is_some());

        let gen = generate_file_id();
        let columns = builder.finish(None);
        let mut writer = AsyncArrowWriter::try_new(
            AsyncWriter::new(
                fs.open_options(
                    &option.table_path(gen, level),
                    FileType::Parquet.open_options(false),
                )
                .await?,
            ),
            schema.arrow_schema().clone(),
            Some(option.write_parquet_properties.clone()),
        )?;
        writer.write(columns.as_record_batch()).await?;
        writer.close().await?;
        version_edits.push(VersionEdit::Add {
            level: level as u8,
            scope: Scope {
                min: min.take().ok_or(CompactionError::EmptyLevel)?,
                max: max.take().ok_or(CompactionError::EmptyLevel)?,
                gen,
                wal_ids: None,
            },
        });
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum CompactionError<R>
where
    R: Record,
{
    #[error("compaction io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("compaction parquet error: {0}")]
    Parquet(#[from] parquet::errors::ParquetError),
    #[error("compaction fusio error: {0}")]
    Fusio(#[from] fusio::Error),
    #[error("compaction version error: {0}")]
    Version(#[from] VersionError<R>),
    #[error("compaction logger error: {0}")]
    Logger(#[from] fusio_log::error::LogError),
    #[error("compaction channel is closed")]
    ChannelClose,
    #[error("database error: {0}")]
    Commit(#[from] CommitError<R>),
    #[error("the level being compacted does not have a table")]
    EmptyLevel,
}

#[cfg(all(test, feature = "tokio"))]
pub(crate) mod tests {
    use std::sync::{atomic::AtomicU32, Arc};

    use flume::bounded;
    use fusio::{path::Path, DynFs};
    use fusio_dispatch::FsOptions;
    use fusio_parquet::writer::AsyncWriter;
    use futures::StreamExt;
    use parquet::arrow::AsyncArrowWriter;
    use parquet_lru::NoCache;
    use tempfile::TempDir;

    use crate::{
        compaction::Compactor,
        context::Context,
        executor::tokio::TokioExecutor,
        fs::{generate_file_id, manager::StoreManager, FileId, FileType},
        inmem::{
            immutable::{tests::TestSchema, Immutable},
            mutable::Mutable,
        },
        record::{DataType, DynRecord, DynSchema, Record, Schema, Value, ValueDesc},
        scope::Scope,
        tests::Test,
        timestamp::Timestamp,
        trigger::{TriggerFactory, TriggerType},
        version::{cleaner::Cleaner, edit::VersionEdit, set::VersionSet, Version, MAX_LEVEL},
        wal::log::LogType,
        DbError, DbOption, DB,
    };

    async fn build_immutable<R>(
        option: &DbOption,
        records: Vec<(LogType, R, Timestamp)>,
        schema: &Arc<R::Schema>,
        fs: &Arc<dyn DynFs>,
    ) -> Result<Immutable<<R::Schema as Schema>::Columns>, DbError<R>>
    where
        R: Record + Send,
    {
        let trigger = TriggerFactory::create(option.trigger_type);

        let mutable: Mutable<R> = Mutable::new(option, trigger, fs, schema.clone()).await?;

        for (log_ty, record, ts) in records {
            let _ = mutable.insert(log_ty, record, ts).await?;
        }
        Ok(mutable.into_immutable().await.unwrap().1)
    }

    pub(crate) async fn build_parquet_table<R>(
        option: &DbOption,
        gen: FileId,
        records: Vec<(LogType, R, Timestamp)>,
        schema: &Arc<R::Schema>,
        level: usize,
        fs: &Arc<dyn DynFs>,
    ) -> Result<(), DbError<R>>
    where
        R: Record + Send,
    {
        let immutable = build_immutable::<R>(option, records, schema, fs).await?;
        let mut writer = AsyncArrowWriter::try_new(
            AsyncWriter::new(
                fs.open_options(
                    &option.table_path(gen, level),
                    FileType::Parquet.open_options(false),
                )
                .await?,
            ),
            schema.arrow_schema().clone(),
            None,
        )?;
        writer.write(immutable.as_record_batch()).await?;
        writer.close().await?;

        Ok(())
    }

    #[tokio::test]
    async fn minor_compaction() {
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_dir_l0 = tempfile::tempdir().unwrap();

        let option = DbOption::new(
            Path::from_filesystem_path(temp_dir.path()).unwrap(),
            &TestSchema,
        )
        .level_path(
            0,
            Path::from_filesystem_path(temp_dir_l0.path()).unwrap(),
            FsOptions::Local,
        )
        .unwrap();
        let manager =
            StoreManager::new(option.base_fs.clone(), option.level_paths.clone()).unwrap();
        manager
            .base_fs()
            .create_dir_all(&option.wal_dir_path())
            .await
            .unwrap();

        let batch_1 = build_immutable::<Test>(
            &option,
            vec![
                (
                    LogType::Full,
                    Test {
                        vstring: 3.to_string(),
                        vu32: 0,
                        vbool: None,
                    },
                    0.into(),
                ),
                (
                    LogType::Full,
                    Test {
                        vstring: 5.to_string(),
                        vu32: 0,
                        vbool: None,
                    },
                    0.into(),
                ),
                (
                    LogType::Full,
                    Test {
                        vstring: 6.to_string(),
                        vu32: 0,
                        vbool: None,
                    },
                    0.into(),
                ),
            ],
            &Arc::new(TestSchema),
            manager.base_fs(),
        )
        .await
        .unwrap();

        let batch_2 = build_immutable::<Test>(
            &option,
            vec![
                (
                    LogType::Full,
                    Test {
                        vstring: 4.to_string(),
                        vu32: 0,
                        vbool: None,
                    },
                    0.into(),
                ),
                (
                    LogType::Full,
                    Test {
                        vstring: 2.to_string(),
                        vu32: 0,
                        vbool: None,
                    },
                    0.into(),
                ),
                (
                    LogType::Full,
                    Test {
                        vstring: 1.to_string(),
                        vu32: 0,
                        vbool: None,
                    },
                    0.into(),
                ),
            ],
            &Arc::new(TestSchema),
            manager.base_fs(),
        )
        .await
        .unwrap();

        let scope = Compactor::<Test>::minor_compaction(
            &option,
            None,
            &vec![
                (Some(generate_file_id()), batch_1),
                (Some(generate_file_id()), batch_2),
            ],
            &TestSchema,
            &manager,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(scope.min, 1.to_string());
        assert_eq!(scope.max, 6.to_string());
    }

    #[tokio::test]
    async fn dyn_minor_compaction() {
        let temp_dir = tempfile::tempdir().unwrap();
        let manager = StoreManager::new(FsOptions::Local, vec![]).unwrap();
        let schema = DynSchema::new(
            vec![ValueDesc::new("id".to_owned(), DataType::Int32, false)],
            0,
        );
        let option = DbOption::new(
            Path::from_filesystem_path(temp_dir.path()).unwrap(),
            &schema,
        );
        manager
            .base_fs()
            .create_dir_all(&option.wal_dir_path())
            .await
            .unwrap();

        let instance = Arc::new(schema);

        let mut batch1_data = vec![];
        let mut batch2_data = vec![];
        for i in 0..40 {
            let col = Value::new(DataType::Int32, "id".to_owned(), Arc::new(i), false);
            if i % 4 == 0 {
                continue;
            }
            if i < 35 && (i % 2 == 0 || i % 5 == 0) {
                batch1_data.push((LogType::Full, DynRecord::new(vec![col], 0), 0.into()));
            } else if i >= 7 {
                batch2_data.push((LogType::Full, DynRecord::new(vec![col], 0), 0.into()));
            }
        }

        // data range: [2, 34]
        let batch_1 =
            build_immutable::<DynRecord>(&option, batch1_data, &instance, manager.base_fs())
                .await
                .unwrap();

        // data range: [7, 39]
        let batch_2 =
            build_immutable::<DynRecord>(&option, batch2_data, &instance, manager.base_fs())
                .await
                .unwrap();

        let scope = Compactor::<DynRecord>::minor_compaction(
            &option,
            None,
            &vec![
                (Some(generate_file_id()), batch_1),
                (Some(generate_file_id()), batch_2),
            ],
            &instance,
            &manager,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            scope.min,
            Value::new(DataType::Int32, "id".to_owned(), Arc::new(2), false)
        );
        assert_eq!(
            scope.max,
            Value::new(DataType::Int32, "id".to_owned(), Arc::new(39), false)
        );
    }

    #[tokio::test]
    async fn major_compaction() {
        let temp_dir = TempDir::new().unwrap();
        let temp_dir_l0 = TempDir::new().unwrap();
        let temp_dir_l1 = TempDir::new().unwrap();

        let mut option = DbOption::new(
            Path::from_filesystem_path(temp_dir.path()).unwrap(),
            &TestSchema,
        )
        .level_path(
            0,
            Path::from_filesystem_path(temp_dir_l0.path()).unwrap(),
            FsOptions::Local,
        )
        .unwrap()
        .level_path(
            1,
            Path::from_filesystem_path(temp_dir_l1.path()).unwrap(),
            FsOptions::Local,
        )
        .unwrap();
        option.major_threshold_with_sst_size = 2;
        let option = Arc::new(option);
        let manager = Arc::new(
            StoreManager::new(option.base_fs.clone(), option.level_paths.clone()).unwrap(),
        );

        manager
            .base_fs()
            .create_dir_all(&option.version_log_dir_path())
            .await
            .unwrap();
        manager
            .base_fs()
            .create_dir_all(&option.wal_dir_path())
            .await
            .unwrap();

        let ((table_gen_1, table_gen_2, table_gen_3, table_gen_4, _), version) =
            build_version(&option, &manager, &Arc::new(TestSchema)).await;

        let min = 2.to_string();
        let max = 5.to_string();
        let mut version_edits = Vec::new();

        let (_, clean_sender) = Cleaner::new(option.clone(), manager.clone());
        let version_set = VersionSet::new(clean_sender, option.clone(), manager.clone())
            .await
            .unwrap();

        let ctx = Context::new(
            manager.clone(),
            Arc::new(NoCache::default()),
            version_set,
            TestSchema.arrow_schema().clone(),
        );
        Compactor::<Test>::major_compaction(
            &version,
            &option,
            &min,
            &max,
            &mut version_edits,
            &mut vec![],
            &TestSchema,
            &ctx,
        )
        .await
        .unwrap();

        if let VersionEdit::Add { level, scope } = &version_edits[0] {
            assert_eq!(*level, 1);
            assert_eq!(scope.min, 1.to_string());
            assert_eq!(scope.max, 6.to_string());
        }
        assert_eq!(
            version_edits[1..5].to_vec(),
            vec![
                VersionEdit::Remove {
                    level: 0,
                    gen: table_gen_1,
                },
                VersionEdit::Remove {
                    level: 0,
                    gen: table_gen_2,
                },
                VersionEdit::Remove {
                    level: 1,
                    gen: table_gen_3,
                },
                VersionEdit::Remove {
                    level: 1,
                    gen: table_gen_4,
                },
            ]
        );
    }

    pub(crate) async fn build_version(
        option: &Arc<DbOption>,
        manager: &StoreManager,
        schema: &Arc<TestSchema>,
    ) -> ((FileId, FileId, FileId, FileId, FileId), Version<Test>) {
        let level_0_fs = option
            .level_fs_path(0)
            .map(|path| manager.get_fs(path))
            .unwrap_or(manager.base_fs());
        let level_1_fs = option
            .level_fs_path(1)
            .map(|path| manager.get_fs(path))
            .unwrap_or(manager.base_fs());

        // level 0
        let table_gen_1 = generate_file_id();
        let table_gen_2 = generate_file_id();
        build_parquet_table::<Test>(
            option,
            table_gen_1,
            vec![
                (
                    LogType::Full,
                    Test {
                        vstring: 1.to_string(),
                        vu32: 0,
                        vbool: Some(true),
                    },
                    1.into(),
                ),
                (
                    LogType::Full,
                    Test {
                        vstring: 2.to_string(),
                        vu32: 0,
                        vbool: Some(true),
                    },
                    1.into(),
                ),
                (
                    LogType::Full,
                    Test {
                        vstring: 3.to_string(),
                        vu32: 0,
                        vbool: Some(true),
                    },
                    0.into(),
                ),
            ],
            schema,
            0,
            level_0_fs,
        )
        .await
        .unwrap();
        build_parquet_table::<Test>(
            option,
            table_gen_2,
            vec![
                (
                    LogType::Full,
                    Test {
                        vstring: 4.to_string(),
                        vu32: 0,
                        vbool: Some(true),
                    },
                    1.into(),
                ),
                (
                    LogType::Full,
                    Test {
                        vstring: 5.to_string(),
                        vu32: 0,
                        vbool: Some(true),
                    },
                    1.into(),
                ),
                (
                    LogType::Full,
                    Test {
                        vstring: 6.to_string(),
                        vu32: 0,
                        vbool: Some(true),
                    },
                    0.into(),
                ),
            ],
            schema,
            0,
            level_0_fs,
        )
        .await
        .unwrap();

        // level 1
        let table_gen_3 = generate_file_id();
        let table_gen_4 = generate_file_id();
        let table_gen_5 = generate_file_id();
        build_parquet_table::<Test>(
            option,
            table_gen_3,
            vec![
                (
                    LogType::Full,
                    Test {
                        vstring: 1.to_string(),
                        vu32: 0,
                        vbool: Some(true),
                    },
                    0.into(),
                ),
                (
                    LogType::Full,
                    Test {
                        vstring: 2.to_string(),
                        vu32: 0,
                        vbool: Some(true),
                    },
                    0.into(),
                ),
                (
                    LogType::Full,
                    Test {
                        vstring: 3.to_string(),
                        vu32: 0,
                        vbool: Some(true),
                    },
                    0.into(),
                ),
            ],
            schema,
            1,
            level_1_fs,
        )
        .await
        .unwrap();
        build_parquet_table::<Test>(
            option,
            table_gen_4,
            vec![
                (
                    LogType::Full,
                    Test {
                        vstring: 4.to_string(),
                        vu32: 0,
                        vbool: Some(true),
                    },
                    0.into(),
                ),
                (
                    LogType::Full,
                    Test {
                        vstring: 5.to_string(),
                        vu32: 0,
                        vbool: Some(true),
                    },
                    0.into(),
                ),
                (
                    LogType::Full,
                    Test {
                        vstring: 6.to_string(),
                        vu32: 0,
                        vbool: Some(true),
                    },
                    0.into(),
                ),
            ],
            schema,
            1,
            level_1_fs,
        )
        .await
        .unwrap();
        build_parquet_table::<Test>(
            option,
            table_gen_5,
            vec![
                (
                    LogType::Full,
                    Test {
                        vstring: 7.to_string(),
                        vu32: 0,
                        vbool: Some(true),
                    },
                    0.into(),
                ),
                (
                    LogType::Full,
                    Test {
                        vstring: 8.to_string(),
                        vu32: 0,
                        vbool: Some(true),
                    },
                    0.into(),
                ),
                (
                    LogType::Full,
                    Test {
                        vstring: 9.to_string(),
                        vu32: 0,
                        vbool: Some(true),
                    },
                    0.into(),
                ),
            ],
            schema,
            1,
            level_1_fs,
        )
        .await
        .unwrap();

        let (sender, _) = bounded(1);
        let mut version =
            Version::<Test>::new(option.clone(), sender, Arc::new(AtomicU32::default()));
        version.level_slice[0].push(Scope {
            min: 1.to_string(),
            max: 3.to_string(),
            gen: table_gen_1,
            wal_ids: None,
        });
        version.level_slice[0].push(Scope {
            min: 4.to_string(),
            max: 6.to_string(),
            gen: table_gen_2,
            wal_ids: None,
        });
        version.level_slice[1].push(Scope {
            min: 1.to_string(),
            max: 3.to_string(),
            gen: table_gen_3,
            wal_ids: None,
        });
        version.level_slice[1].push(Scope {
            min: 4.to_string(),
            max: 6.to_string(),
            gen: table_gen_4,
            wal_ids: None,
        });
        version.level_slice[1].push(Scope {
            min: 7.to_string(),
            max: 9.to_string(),
            gen: table_gen_5,
            wal_ids: None,
        });
        (
            (
                table_gen_1,
                table_gen_2,
                table_gen_3,
                table_gen_4,
                table_gen_5,
            ),
            version,
        )
    }

    // https://github.com/tonbo-io/tonbo/pull/139
    #[tokio::test]
    pub(crate) async fn major_panic() {
        let temp_dir = TempDir::new().unwrap();

        let mut option = DbOption::new(
            Path::from_filesystem_path(temp_dir.path()).unwrap(),
            &TestSchema,
        );
        option.major_threshold_with_sst_size = 1;
        option.level_sst_magnification = 1;
        let manager = Arc::new(
            StoreManager::new(option.base_fs.clone(), option.level_paths.clone()).unwrap(),
        );

        manager
            .base_fs()
            .create_dir_all(&option.version_log_dir_path())
            .await
            .unwrap();
        manager
            .base_fs()
            .create_dir_all(&option.wal_dir_path())
            .await
            .unwrap();

        let level_0_fs = option
            .level_fs_path(0)
            .map(|path| manager.get_fs(path))
            .unwrap_or(manager.base_fs());
        let level_1_fs = option
            .level_fs_path(1)
            .map(|path| manager.get_fs(path))
            .unwrap_or(manager.base_fs());

        let table_gen0 = generate_file_id();
        let table_gen1 = generate_file_id();
        let mut records0 = vec![];
        let mut records1 = vec![];
        for i in 0..10 {
            let record = (
                LogType::Full,
                Test {
                    vstring: i.to_string(),
                    vu32: i,
                    vbool: Some(true),
                },
                0.into(),
            );
            if i < 5 {
                records0.push(record);
            } else {
                records1.push(record);
            }
        }
        build_parquet_table::<Test>(
            &option,
            table_gen0,
            records0,
            &Arc::new(TestSchema),
            0,
            level_0_fs,
        )
        .await
        .unwrap();
        build_parquet_table::<Test>(
            &option,
            table_gen1,
            records1,
            &Arc::new(TestSchema),
            1,
            level_1_fs,
        )
        .await
        .unwrap();

        let option = Arc::new(option);
        let (sender, _) = bounded(1);
        let mut version =
            Version::<Test>::new(option.clone(), sender, Arc::new(AtomicU32::default()));
        version.level_slice[0].push(Scope {
            min: 0.to_string(),
            max: 4.to_string(),
            gen: table_gen0,
            wal_ids: None,
        });
        version.level_slice[1].push(Scope {
            min: 5.to_string(),
            max: 9.to_string(),
            gen: table_gen1,
            wal_ids: None,
        });

        let mut version_edits = Vec::new();
        let min = 6.to_string();
        let max = 9.to_string();

        let (_, clean_sender) = Cleaner::new(option.clone(), manager.clone());
        let version_set = VersionSet::new(clean_sender, option.clone(), manager.clone())
            .await
            .unwrap();
        let ctx = Context::new(
            manager.clone(),
            Arc::new(NoCache::default()),
            version_set,
            TestSchema.arrow_schema().clone(),
        );
        Compactor::<Test>::major_compaction(
            &version,
            &option,
            &min,
            &max,
            &mut version_edits,
            &mut vec![],
            &TestSchema,
            &ctx,
        )
        .await
        .unwrap();
    }

    // issue: https://github.com/tonbo-io/tonbo/issues/152
    #[tokio::test]
    async fn test_flush_major_level_sort() {
        let temp_dir = TempDir::new().unwrap();

        let mut option = DbOption::new(
            Path::from_filesystem_path(temp_dir.path()).unwrap(),
            &TestSchema,
        );
        option.immutable_chunk_num = 1;
        option.immutable_chunk_max_num = 0;
        option.major_threshold_with_sst_size = 2;
        option.level_sst_magnification = 1;

        option.max_sst_file_size = 2 * 1024 * 1024;
        option.major_default_oldest_table_num = 1;
        option.trigger_type = TriggerType::Length(5);

        let db: DB<Test, TokioExecutor> = DB::new(option, TokioExecutor::current(), TestSchema)
            .await
            .unwrap();

        for i in 5..9 {
            let item = Test {
                vstring: i.to_string(),
                vu32: i,
                vbool: Some(true),
            };
            db.insert(item).await.unwrap();
        }

        db.flush().await.unwrap();
        for i in 0..4 {
            let item = Test {
                vstring: i.to_string(),
                vu32: i,
                vbool: Some(true),
            };
            db.insert(item).await.unwrap();
        }

        db.flush().await.unwrap();

        db.insert(Test {
            vstring: "6".to_owned(),
            vu32: 22,
            vbool: Some(false),
        })
        .await
        .unwrap();
        db.insert(Test {
            vstring: "8".to_owned(),
            vu32: 77,
            vbool: Some(false),
        })
        .await
        .unwrap();
        db.flush().await.unwrap();
        db.insert(Test {
            vstring: "1".to_owned(),
            vu32: 22,
            vbool: Some(false),
        })
        .await
        .unwrap();
        db.insert(Test {
            vstring: "5".to_owned(),
            vu32: 77,
            vbool: Some(false),
        })
        .await
        .unwrap();
        db.flush().await.unwrap();

        db.insert(Test {
            vstring: "2".to_owned(),
            vu32: 22,
            vbool: Some(false),
        })
        .await
        .unwrap();
        db.insert(Test {
            vstring: "7".to_owned(),
            vu32: 77,
            vbool: Some(false),
        })
        .await
        .unwrap();
        db.flush().await.unwrap();

        let version = db.ctx.version_set.current().await;

        for level in 0..MAX_LEVEL {
            let sort_runs = &version.level_slice[level];

            if sort_runs.is_empty() {
                continue;
            }
            for pos in 0..sort_runs.len() - 1 {
                let current = &sort_runs[pos];
                let next = &sort_runs[pos + 1];

                assert!(current.min < current.max);
                assert!(next.min < next.max);

                if level == 0 {
                    continue;
                }
                assert!(current.max < next.min);
            }
        }
        dbg!(version);
    }

    #[ignore = "s3"]
    #[tokio::test]
    async fn test_recover_before_flush_from_s3() {
        if option_env!("AWS_ACCESS_KEY_ID").is_none()
            || option_env!("AWS_SECRET_ACCESS_KEY").is_none()
        {
            return;
        }
        let key_id = std::option_env!("AWS_ACCESS_KEY_ID").unwrap().to_string();
        let secret_key = std::option_env!("AWS_SECRET_ACCESS_KEY")
            .unwrap()
            .to_string();

        let s3_option = FsOptions::S3 {
            bucket: "fusio-test".to_string(),
            credential: Some(fusio::remotes::aws::AwsCredential {
                key_id,
                secret_key,
                token: None,
            }),
            endpoint: None,
            region: Some("ap-southeast-1".to_string()),
            sign_payload: None,
            checksum: None,
        };
        let temp_dir = tempfile::tempdir().unwrap();

        let mut option = DbOption::new(
            Path::from_filesystem_path(temp_dir.path()).unwrap(),
            &TestSchema,
        )
        .base_fs(s3_option);
        option.version_log_snapshot_threshold = 100000;
        {
            let manager =
                StoreManager::new(option.base_fs.clone(), option.level_paths.clone()).unwrap();
            manager
                .base_fs()
                .create_dir_all(&option.wal_dir_path())
                .await
                .unwrap();
            manager
                .local_fs()
                .create_dir_all(&option.wal_dir_path())
                .await
                .unwrap();

            let batch_1 = build_immutable::<Test>(
                &option,
                vec![
                    (
                        LogType::Full,
                        Test {
                            vstring: 3.to_string(),
                            vu32: 0,
                            vbool: None,
                        },
                        0.into(),
                    ),
                    (
                        LogType::Full,
                        Test {
                            vstring: 5.to_string(),
                            vu32: 0,
                            vbool: None,
                        },
                        0.into(),
                    ),
                    (
                        LogType::Full,
                        Test {
                            vstring: 6.to_string(),
                            vu32: 0,
                            vbool: None,
                        },
                        0.into(),
                    ),
                ],
                &Arc::new(TestSchema),
                manager.base_fs(),
            )
            .await
            .unwrap();

            let batch_2 = build_immutable::<Test>(
                &option,
                vec![
                    (
                        LogType::Full,
                        Test {
                            vstring: 4.to_string(),
                            vu32: 0,
                            vbool: None,
                        },
                        0.into(),
                    ),
                    (
                        LogType::Full,
                        Test {
                            vstring: 2.to_string(),
                            vu32: 0,
                            vbool: None,
                        },
                        0.into(),
                    ),
                    (
                        LogType::Full,
                        Test {
                            vstring: 1.to_string(),
                            vu32: 0,
                            vbool: None,
                        },
                        0.into(),
                    ),
                ],
                &Arc::new(TestSchema),
                manager.base_fs(),
            )
            .await
            .unwrap();

            let scope = Compactor::<Test>::minor_compaction(
                &option,
                None,
                &vec![
                    (Some(generate_file_id()), batch_1),
                    (Some(generate_file_id()), batch_2),
                ],
                &TestSchema,
                &manager,
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(scope.min, 1.to_string());
            assert_eq!(scope.max, 6.to_string());
        }
        // test recover from s3
        {
            let db: DB<Test, TokioExecutor> =
                DB::new(option.clone(), TokioExecutor::current(), TestSchema)
                    .await
                    .unwrap();
            let mut expected_key = (1..=6).map(|v| v.to_string()).collect::<Vec<String>>();
            let tx = db.transaction().await;
            let mut scan = tx
                .scan((std::ops::Bound::Unbounded, std::ops::Bound::Unbounded))
                .take()
                .await
                .unwrap();

            while let Some(actual) = scan.next().await.transpose().unwrap() {
                let expected = expected_key.remove(0);
                assert_eq!(actual.key().value, &expected);
            }
            assert!(expected_key.is_empty());
        }
    }
}
