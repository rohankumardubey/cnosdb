#![allow(clippy::too_many_arguments)]
use coordinator::{reader::ReaderIterator, service::CoordinatorRef};
use std::task::Poll;

use datafusion::{
    arrow::{datatypes::SchemaRef, error::ArrowError, record_batch::RecordBatch},
    physical_plan::RecordBatchStream,
};
use futures::{executor::block_on, FutureExt, Stream};
use models::codec::Encoding;
use models::schema::TskvTableSchemaRef;
use models::{
    predicate::domain::PredicateRef,
    schema::{ColumnType, TableColumn, TskvTableSchema, TIME_FIELD},
};

use spi::{QueryError, Result};
use tskv::iterator::{QueryOption, TableScanMetrics};

#[allow(dead_code)]
pub struct TableScanStream {
    proj_schema: SchemaRef,
    batch_size: usize,
    coord: CoordinatorRef,

    iterator: ReaderIterator,

    metrics: TableScanMetrics,
}

impl TableScanStream {
    pub fn new(
        table_schema: TskvTableSchemaRef,
        proj_schema: SchemaRef,
        coord: CoordinatorRef,
        filter: PredicateRef,
        batch_size: usize,
        metrics: TableScanMetrics,
    ) -> Result<Self> {
        let mut proj_fileds = Vec::with_capacity(proj_schema.fields().len());
        for item in proj_schema.fields().iter() {
            let field_name = item.name();
            if field_name == TIME_FIELD {
                let encoding = match table_schema.column(TIME_FIELD) {
                    None => Encoding::Default,
                    Some(v) => v.encoding,
                };
                proj_fileds.push(TableColumn::new(
                    0,
                    TIME_FIELD.to_string(),
                    ColumnType::Time,
                    encoding,
                ));
                continue;
            }

            if let Some(v) = table_schema.column(field_name) {
                proj_fileds.push(v.clone());
            } else {
                return Err(QueryError::CommonError {
                    msg: format!(
                        "table stream build fail, because can't found field: {}",
                        field_name
                    ),
                });
            }
        }

        let proj_table_schema = TskvTableSchema::new(
            table_schema.tenant.clone(),
            table_schema.db.clone(),
            table_schema.name.clone(),
            proj_fileds,
        );

        let option = QueryOption::new(
            batch_size,
            table_schema.tenant.clone(),
            filter,
            proj_schema.clone(),
            proj_table_schema,
            metrics.tskv_metrics(),
        );

        let iterator = block_on(coord.read_record(option))?;

        Ok(Self {
            proj_schema,
            batch_size,
            coord,
            iterator,
            metrics,
        })
    }
}

impl Stream for TableScanStream {
    type Item = std::result::Result<RecordBatch, ArrowError>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        let timer = this.metrics.elapsed_compute().timer();

        let result = match Box::pin(this.iterator.next()).poll_unpin(cx) {
            Poll::Ready(Some(Ok(record_batch))) => Poll::Ready(Some(Ok(record_batch))),
            Poll::Ready(Some(Err(e))) => {
                Poll::Ready(Some(Err(ArrowError::ExternalError(Box::new(e)))))
            }
            Poll::Ready(None) => {
                this.metrics.done();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        };

        timer.done();
        this.metrics.record_poll(result)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        // todo   (self.data.len(), Some(self.data.len()))
        (0, Some(0))
    }
}

impl RecordBatchStream for TableScanStream {
    fn schema(&self) -> SchemaRef {
        self.proj_schema.clone()
    }
}
