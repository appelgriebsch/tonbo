use std::{ops::Bound, sync::Arc};

use async_lock::Mutex;
use crossbeam_skiplist::{
    map::{Entry, Range},
    SkipMap,
};
use fusio::DynFs;

use crate::{
    fs::{generate_file_id, FileId},
    inmem::immutable::Immutable,
    record::{KeyRef, Record, Schema},
    timestamp::{
        timestamped::{Timestamped, TimestampedRef},
        Timestamp, EPOCH,
    },
    trigger::Trigger,
    wal::{
        log::{Log, LogType},
        WalFile,
    },
    DbError, DbOption,
};

pub(crate) type MutableScan<'scan, R> = Range<
    'scan,
    TimestampedRef<<<R as Record>::Schema as Schema>::Key>,
    (
        Bound<&'scan TimestampedRef<<<R as Record>::Schema as Schema>::Key>>,
        Bound<&'scan TimestampedRef<<<R as Record>::Schema as Schema>::Key>>,
    ),
    Timestamped<<<R as Record>::Schema as Schema>::Key>,
    Option<R>,
>;

pub struct Mutable<R>
where
    R: Record,
{
    pub(crate) data: SkipMap<Timestamped<<R::Schema as Schema>::Key>, Option<R>>,
    wal: Option<Mutex<WalFile<R>>>,
    pub(crate) trigger: Arc<dyn Trigger<R>>,
    pub(super) schema: Arc<R::Schema>,
}

impl<R> Mutable<R>
where
    R: Record,
{
    pub async fn new(
        option: &DbOption,
        trigger: Arc<dyn Trigger<R>>,
        fs: &Arc<dyn DynFs>,
        schema: Arc<R::Schema>,
    ) -> Result<Self, fusio::Error> {
        let mut wal = None;
        if option.use_wal {
            let file_id = generate_file_id();

            wal = Some(Mutex::new(
                WalFile::<R>::new(
                    fs.clone(),
                    option.wal_path(file_id),
                    option.wal_buffer_size,
                    file_id,
                )
                .await,
            ));
        };

        Ok(Self {
            data: Default::default(),
            wal,
            trigger,
            schema,
        })
    }

    pub(crate) async fn destroy(&mut self) -> Result<(), DbError<R>> {
        if let Some(wal) = self.wal.take() {
            wal.into_inner().remove().await?;
        }
        Ok(())
    }
}

impl<R> Mutable<R>
where
    R: Record + Send,
{
    pub(crate) async fn insert(
        &self,
        log_ty: LogType,
        record: R,
        ts: Timestamp,
    ) -> Result<bool, DbError<R>> {
        self.append(Some(log_ty), record.key().to_key(), ts, Some(record))
            .await
    }

    pub(crate) async fn remove(
        &self,
        log_ty: LogType,
        key: <R::Schema as Schema>::Key,
        ts: Timestamp,
    ) -> Result<bool, DbError<R>> {
        self.append(Some(log_ty), key, ts, None).await
    }

    pub(crate) async fn append(
        &self,
        log_ty: Option<LogType>,
        key: <R::Schema as Schema>::Key,
        ts: Timestamp,
        value: Option<R>,
    ) -> Result<bool, DbError<R>> {
        let timestamped_key = Timestamped::new(key, ts);

        let record_entry = Log::new(timestamped_key, value, log_ty);
        if let (Some(_log_ty), Some(wal)) = (log_ty, &self.wal) {
            let mut wal_guard = wal.lock().await;

            wal_guard
                .write(&record_entry)
                .await
                .map_err(|e| DbError::WalWrite(Box::new(e)))?;
        }

        let is_exceeded = self.trigger.check_if_exceed(&record_entry.value);
        self.data.insert(record_entry.key, record_entry.value);

        Ok(is_exceeded)
    }

    pub(crate) fn get(
        &self,
        key: &<R::Schema as Schema>::Key,
        ts: Timestamp,
    ) -> Option<Entry<'_, Timestamped<<R::Schema as Schema>::Key>, Option<R>>> {
        self.data
            .range::<TimestampedRef<<R::Schema as Schema>::Key>, _>((
                Bound::Included(TimestampedRef::new(key, ts)),
                Bound::Included(TimestampedRef::new(key, EPOCH)),
            ))
            .next()
    }

    pub(crate) fn scan<'scan>(
        &'scan self,
        range: (
            Bound<&'scan <R::Schema as Schema>::Key>,
            Bound<&'scan <R::Schema as Schema>::Key>,
        ),
        ts: Timestamp,
    ) -> MutableScan<'scan, R> {
        let lower = match range.0 {
            Bound::Included(key) => Bound::Included(TimestampedRef::new(key, ts)),
            Bound::Excluded(key) => Bound::Excluded(TimestampedRef::new(key, EPOCH)),
            Bound::Unbounded => Bound::Unbounded,
        };
        let upper = match range.1 {
            Bound::Included(key) => Bound::Included(TimestampedRef::new(key, EPOCH)),
            Bound::Excluded(key) => Bound::Excluded(TimestampedRef::new(key, ts)),
            Bound::Unbounded => Bound::Unbounded,
        };

        self.data.range((lower, upper))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub(crate) fn check_conflict(&self, key: &<R::Schema as Schema>::Key, ts: Timestamp) -> bool {
        self.data
            .range::<TimestampedRef<<R::Schema as Schema>::Key>, _>((
                Bound::Excluded(TimestampedRef::new(key, u32::MAX.into())),
                Bound::Excluded(TimestampedRef::new(key, ts)),
            ))
            .next()
            .is_some()
    }

    pub(crate) async fn into_immutable(
        self,
    ) -> Result<
        (Option<FileId>, Immutable<<R::Schema as Schema>::Columns>),
        fusio_log::error::LogError,
    > {
        let mut file_id = None;

        if let Some(wal) = self.wal {
            let mut wal_guard = wal.lock().await;
            wal_guard.flush().await?;
            file_id = Some(wal_guard.file_id());
        }

        Ok((
            file_id,
            Immutable::new(self.data, self.schema.arrow_schema().clone()),
        ))
    }

    pub(crate) async fn flush_wal(&self) -> Result<(), DbError<R>> {
        if let Some(wal) = self.wal.as_ref() {
            let mut wal_guard = wal.lock().await;
            wal_guard.flush().await?;
        }
        Ok(())
    }
}

impl<R> Mutable<R>
where
    R: Record,
{
    #[allow(unused)]
    pub(crate) fn len(&self) -> usize {
        self.data.len()
    }
}

#[cfg(all(test, feature = "tokio"))]
mod tests {
    use std::{ops::Bound, sync::Arc};

    use fusio::{disk::TokioFs, path::Path, DynFs};

    use super::Mutable;
    use crate::{
        inmem::immutable::tests::TestSchema,
        record::{test::StringSchema, Datatype, DynRecord, DynSchema, Record, Value, ValueDesc},
        tests::{Test, TestRef},
        timestamp::Timestamped,
        trigger::TriggerFactory,
        wal::log::LogType,
        DbOption,
    };

    #[tokio::test]
    async fn insert_and_get() {
        let key_1 = "key_1".to_owned();
        let key_2 = "key_2".to_owned();

        let temp_dir = tempfile::tempdir().unwrap();
        let fs = Arc::new(TokioFs) as Arc<dyn DynFs>;
        let option = DbOption::new(
            Path::from_filesystem_path(temp_dir.path()).unwrap(),
            &TestSchema,
        );
        fs.create_dir_all(&option.wal_dir_path()).await.unwrap();

        let trigger = TriggerFactory::create(option.trigger_type);
        let mem_table = Mutable::<Test>::new(&option, trigger, &fs, Arc::new(TestSchema {}))
            .await
            .unwrap();

        mem_table
            .insert(
                LogType::Full,
                Test {
                    vstring: key_1.clone(),
                    vu32: 1,
                    vbool: Some(true),
                },
                0_u32.into(),
            )
            .await
            .unwrap();
        mem_table
            .insert(
                LogType::Full,
                Test {
                    vstring: key_2.clone(),
                    vu32: 2,
                    vbool: None,
                },
                1_u32.into(),
            )
            .await
            .unwrap();

        let entry = mem_table.get(&key_1, 0_u32.into()).unwrap();
        assert_eq!(
            entry.value().as_ref().unwrap().as_record_ref(),
            TestRef {
                vstring: &key_1,
                vu32: Some(1),
                vbool: Some(true)
            }
        );
        assert!(mem_table.get(&key_2, 0_u32.into()).is_none());
        assert!(mem_table.get(&key_2, 1_u32.into()).is_some());
    }

    #[tokio::test]
    async fn range() {
        let temp_dir = tempfile::tempdir().unwrap();
        let fs = Arc::new(TokioFs) as Arc<dyn DynFs>;
        let option = DbOption::new(
            Path::from_filesystem_path(temp_dir.path()).unwrap(),
            &StringSchema,
        );
        fs.create_dir_all(&option.wal_dir_path()).await.unwrap();

        let trigger = TriggerFactory::create(option.trigger_type);

        let mutable = Mutable::<String>::new(&option, trigger, &fs, Arc::new(StringSchema))
            .await
            .unwrap();

        mutable
            .insert(LogType::Full, "1".into(), 0_u32.into())
            .await
            .unwrap();
        mutable
            .insert(LogType::Full, "2".into(), 0_u32.into())
            .await
            .unwrap();
        mutable
            .insert(LogType::Full, "2".into(), 1_u32.into())
            .await
            .unwrap();
        mutable
            .insert(LogType::Full, "3".into(), 1_u32.into())
            .await
            .unwrap();
        mutable
            .insert(LogType::Full, "4".into(), 0_u32.into())
            .await
            .unwrap();

        let mut scan = mutable.scan((Bound::Unbounded, Bound::Unbounded), 0_u32.into());

        assert_eq!(
            scan.next().unwrap().key(),
            &Timestamped::new("1".into(), 0_u32.into())
        );
        assert_eq!(
            scan.next().unwrap().key(),
            &Timestamped::new("2".into(), 1_u32.into())
        );
        assert_eq!(
            scan.next().unwrap().key(),
            &Timestamped::new("2".into(), 0_u32.into())
        );
        assert_eq!(
            scan.next().unwrap().key(),
            &Timestamped::new("3".into(), 1_u32.into())
        );
        assert_eq!(
            scan.next().unwrap().key(),
            &Timestamped::new("4".into(), 0_u32.into())
        );

        let lower = "1".to_string();
        let upper = "4".to_string();
        let mut scan = mutable.scan(
            (Bound::Included(&lower), Bound::Included(&upper)),
            1_u32.into(),
        );

        assert_eq!(
            scan.next().unwrap().key(),
            &Timestamped::new("1".into(), 0_u32.into())
        );
        assert_eq!(
            scan.next().unwrap().key(),
            &Timestamped::new("2".into(), 1_u32.into())
        );
        assert_eq!(
            scan.next().unwrap().key(),
            &Timestamped::new("2".into(), 0_u32.into())
        );
        assert_eq!(
            scan.next().unwrap().key(),
            &Timestamped::new("3".into(), 1_u32.into())
        );
        assert_eq!(
            scan.next().unwrap().key(),
            &Timestamped::new("4".into(), 0_u32.into())
        );
    }

    #[tokio::test]
    async fn test_dyn_read() {
        let temp_dir = tempfile::tempdir().unwrap();
        let schema = DynSchema::new(
            vec![
                ValueDesc::new("age".to_string(), Datatype::Int8, false),
                ValueDesc::new("height".to_string(), Datatype::Int16, true),
            ],
            0,
        );
        let option = DbOption::new(
            Path::from_filesystem_path(temp_dir.path()).unwrap(),
            &schema,
        );
        let fs = Arc::new(TokioFs) as Arc<dyn DynFs>;
        fs.create_dir_all(&option.wal_dir_path()).await.unwrap();

        let trigger = TriggerFactory::create(option.trigger_type);

        let schema = Arc::new(schema);

        let mutable = Mutable::<DynRecord>::new(&option, trigger, &fs, schema)
            .await
            .unwrap();

        mutable
            .insert(
                LogType::Full,
                DynRecord::new(
                    vec![
                        Value::new(Datatype::Int8, "age".to_string(), Arc::new(1_i8), false),
                        Value::new(
                            Datatype::Int16,
                            "height".to_string(),
                            Arc::new(1236_i16),
                            true,
                        ),
                    ],
                    0,
                ),
                0_u32.into(),
            )
            .await
            .unwrap();

        {
            let mut scan = mutable.scan((Bound::Unbounded, Bound::Unbounded), 0_u32.into());
            let entry = scan.next().unwrap();
            assert_eq!(
                entry.key(),
                &Timestamped::new(
                    Value::new(Datatype::Int8, "age".to_string(), Arc::new(1_i8), false),
                    0_u32.into()
                )
            );
            dbg!(entry.clone().value().as_ref().unwrap());
        }
    }
}
