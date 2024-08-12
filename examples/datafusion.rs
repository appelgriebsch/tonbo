use std::{
    any::Any,
    collections::Bound,
    fmt::{Debug, Formatter},
    pin::{pin, Pin},
    sync::Arc,
    task::{Context, Poll},
};

use arrow::{datatypes::SchemaRef, record_batch::RecordBatch, util::pretty};
use async_stream::stream;
use async_trait::async_trait;
use datafusion::{
    common::internal_err,
    datasource::{TableProvider, TableType},
    error::{DataFusionError, Result},
    execution::{context::SessionState, RecordBatchStream, SendableRecordBatchStream, TaskContext},
    physical_expr::EquivalenceProperties,
    physical_plan::{DisplayAs, DisplayFormatType, ExecutionMode, ExecutionPlan, PlanProperties},
    prelude::*,
};
use futures_core::Stream;
use futures_util::StreamExt;
use tonbo::{executor::tokio::TokioExecutor, inmem::immutable::ArrowArrays, record::Record, DB};
use tonbo_marco::tonbo_record;

#[tonbo_record]
pub struct Music {
    #[primary_key]
    id: u64,
    name: String,
    like: i64,
}

struct MusicProvider {
    db: Arc<DB<Music, TokioExecutor>>,
}

struct MusicExec {
    cache: PlanProperties,
    db: Arc<DB<Music, TokioExecutor>>,
    projection: Option<Vec<usize>>,
    limit: Option<usize>,
    range: (Bound<<Music as Record>::Key>, Bound<<Music as Record>::Key>),
}

struct MusicStream {
    stream: Pin<Box<dyn Stream<Item = Result<RecordBatch, DataFusionError>> + Send>>,
}

#[async_trait]
impl TableProvider for MusicProvider {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        Music::arrow_schema().clone()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    async fn scan(
        &self,
        _: &SessionState,
        projection: Option<&Vec<usize>>,
        _filters: &[Expr],
        limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let mut exec = MusicExec::new(self.db.clone());

        // TODO: filters to range detach
        // exec.range =
        exec.projection = projection.cloned();
        if let Some(projection) = exec.projection.as_mut() {
            for index in projection {
                *index = index.checked_sub(2).unwrap_or(0);
            }
        }

        exec.limit = limit;

        Ok(Arc::new(exec))
    }
}

impl MusicExec {
    fn new(db: Arc<DB<Music, TokioExecutor>>) -> Self {
        MusicExec {
            cache: PlanProperties::new(
                EquivalenceProperties::new_with_orderings(Music::arrow_schema().clone(), &[]),
                datafusion::physical_expr::Partitioning::UnknownPartitioning(1),
                ExecutionMode::Unbounded,
            ),
            db,
            projection: None,
            limit: None,
            range: (Bound::Unbounded, Bound::Unbounded),
        }
    }
}

impl Stream for MusicStream {
    type Item = Result<RecordBatch>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        pin!(&mut self.stream).poll_next(cx)
    }
}

impl RecordBatchStream for MusicStream {
    fn schema(&self) -> SchemaRef {
        Music::arrow_schema().clone()
    }
}

impl DisplayAs for MusicExec {
    fn fmt_as(&self, _: DisplayFormatType, f: &mut Formatter) -> std::fmt::Result {
        let (lower, upper) = self.range;

        write!(
            f,
            "MusicExec: range:({:?}, {:?}), projection: [{:?}], limit: {:?}",
            lower, upper, self.projection, self.limit
        )
    }
}

impl Debug for MusicExec {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MusicExec")
            .field("cache", &self.cache)
            .field("limit", &self.limit)
            .field("projection", &self.projection)
            .field("range", &self.range)
            .finish()
    }
}

impl ExecutionPlan for MusicExec {
    fn name(&self) -> &str {
        "MusicExec"
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn properties(&self) -> &PlanProperties {
        &self.cache
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        vec![]
    }

    fn with_new_children(
        self: Arc<Self>,
        children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        if children.is_empty() {
            Ok(self)
        } else {
            internal_err!("Children cannot be replaced in {self:?}")
        }
    }

    fn execute(&self, _: usize, _: Arc<TaskContext>) -> Result<SendableRecordBatchStream> {
        let db = self.db.clone();
        let (lower, upper) = self.range.clone();
        let limit = self.limit.clone();
        let projection = self.projection.clone();

        Ok(Box::pin(MusicStream {
            stream: Box::pin(stream! {
                let txn = db.transaction().await;

                let mut scan = txn
                    .scan((lower.as_ref(), upper.as_ref()))
                    .await;
                if let Some(limit) = limit {
                    scan = scan.limit(limit);
                }
                if let Some(projection) = projection {
                    scan = scan.projection(projection.clone());
                }
                let mut scan = scan.package(8192).await.map_err(|err| DataFusionError::Internal(err.to_string()))?;

                while let Some(record) = scan.next().await {
                    yield Ok(record?.as_record_batch().clone())
                }
            }),
        }))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let db = DB::new("./db_path/music".into(), TokioExecutor::default())
        .await
        .unwrap();
    for (id, name, like) in vec![
        (0, "welcome".to_string(), 0),
        (1, "tonbo".to_string(), 999),
        (2, "star".to_string(), 233),
        (3, "plz".to_string(), 2),
    ] {
        db.insert(Music { id, name, like }).await.unwrap();
    }
    let ctx = SessionContext::new();

    let provider = MusicProvider { db: Arc::new(db) };
    ctx.register_table("music", Arc::new(provider))?;

    let df = ctx.table("music").await?;
    let df = df.select(vec![col("name")])?;
    let batches = df.collect().await?;
    pretty::print_batches(&batches).unwrap();
    Ok(())
}