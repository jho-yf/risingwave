// Copyright 2023 RisingWave Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::Arc;
use std::time::Instant;

use futures::stream::select;
use futures::{FutureExt, StreamExt};
use futures_async_stream::try_stream;
use itertools::Itertools;
use prometheus::Histogram;
use risingwave_common::array::stream_chunk::StreamChunkMut;
use risingwave_common::array::{merge_chunk_row, Op, StreamChunk};
use risingwave_common::catalog::{ColumnCatalog, Field, Schema};
use risingwave_common::util::epoch::EpochPair;
use risingwave_connector::dispatch_sink;
use risingwave_connector::sink::catalog::SinkType;
use risingwave_connector::sink::{
    build_sink, Sink, SinkImpl, SinkParam, SinkWriter, SinkWriterParam,
};

use super::error::{StreamExecutorError, StreamExecutorResult};
use super::{BoxedExecutor, Executor, Message, PkIndices};
use crate::common::log_store::{
    LogReader, LogStoreFactory, LogStoreReadItem, LogWriter, TruncateOffset,
};
use crate::executor::monitor::StreamingMetrics;
use crate::executor::{expect_first_barrier, ActorContextRef, BoxedMessageStream};

pub struct SinkExecutor<F: LogStoreFactory> {
    input: BoxedExecutor,
    metrics: Arc<StreamingMetrics>,
    sink: SinkImpl,
    identity: String,
    pk_indices: PkIndices,
    input_columns: Vec<ColumnCatalog>,
    input_schema: Schema,
    sink_param: SinkParam,
    actor_context: ActorContextRef,
    log_reader: F::Reader,
    log_writer: F::Writer,
    sink_writer_param: SinkWriterParam,
}

struct SinkMetrics {
    sink_commit_duration_metrics: Histogram,
}

// Drop all the DELETE messages in this chunk and convert UPDATE INSERT into INSERT.
fn force_append_only(c: StreamChunk) -> StreamChunk {
    let mut c: StreamChunkMut = c.into();
    for (_, mut r) in c.to_rows_mut() {
        match r.op() {
            Op::Insert => {}
            Op::Delete | Op::UpdateDelete => r.set_vis(false),
            Op::UpdateInsert => r.set_op(Op::Insert),
        }
    }
    c.into()
}

impl<F: LogStoreFactory> SinkExecutor<F> {
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        input: BoxedExecutor,
        metrics: Arc<StreamingMetrics>,
        sink_writer_param: SinkWriterParam,
        sink_param: SinkParam,
        columns: Vec<ColumnCatalog>,
        actor_context: ActorContextRef,
        log_store_factory: F,
        pk_indices: PkIndices,
    ) -> StreamExecutorResult<Self> {
        let (log_reader, log_writer) = log_store_factory.build().await;

        let sink = build_sink(sink_param.clone())?;
        let input_schema = columns
            .iter()
            .map(|column| Field::from(&column.column_desc))
            .collect();
        Ok(Self {
            input,
            metrics,
            sink,
            identity: format!("SinkExecutor {:X?}", sink_writer_param.executor_id),
            pk_indices,
            input_columns: columns,
            input_schema,
            sink_param,
            actor_context,
            log_reader,
            log_writer,
            sink_writer_param,
        })
    }

    fn execute_inner(self) -> BoxedMessageStream {
        let sink_commit_duration_metrics = self
            .metrics
            .sink_commit_duration
            .with_label_values(&[self.identity.as_str(), self.sink.get_connector()]);

        let sink_metrics = SinkMetrics {
            sink_commit_duration_metrics,
        };

        let write_log_stream = Self::execute_write_log(
            self.input,
            self.pk_indices,
            self.log_writer,
            self.sink_param.sink_type,
            self.actor_context,
        );

        dispatch_sink!(self.sink, sink, {
            let consume_log_stream = Self::execute_consume_log(
                sink,
                self.log_reader,
                self.input_columns,
                sink_metrics,
                self.sink_writer_param,
            );
            select(consume_log_stream.into_stream(), write_log_stream).boxed()
        })
    }

    #[try_stream(ok = Message, error = StreamExecutorError)]
    async fn execute_write_log(
        input: BoxedExecutor,
        stream_key: PkIndices,
        mut log_writer: impl LogWriter,
        sink_type: SinkType,
        actor_context: ActorContextRef,
    ) {
        let mut input = input.execute();

        let barrier = expect_first_barrier(&mut input).await?;

        let epoch_pair = barrier.epoch;

        log_writer
            .init(EpochPair::new_test_epoch(epoch_pair.curr))
            .await?;

        // Propagate the first barrier
        yield Message::Barrier(barrier);

        #[for_await]
        for msg in input {
            match msg? {
                Message::Watermark(w) => yield Message::Watermark(w),
                Message::Chunk(chunk) => {
                    // Compact the chunk to eliminate any useless intermediate result (e.g. UPDATE
                    // V->V).
                    let chunk = merge_chunk_row(chunk, &stream_key);
                    let chunk = if sink_type == SinkType::ForceAppendOnly {
                        // Force append-only by dropping UPDATE/DELETE messages. We do this when the
                        // user forces the sink to be append-only while it is actually not based on
                        // the frontend derivation result.
                        force_append_only(chunk)
                    } else {
                        chunk
                    };

                    log_writer.write_chunk(chunk.clone()).await?;

                    // Use original chunk instead of the reordered one as the executor output.
                    yield Message::Chunk(chunk);
                }
                Message::Barrier(barrier) => {
                    log_writer
                        .flush_current_epoch(barrier.epoch.curr, barrier.kind.is_checkpoint())
                        .await?;
                    if let Some(vnode_bitmap) = barrier.as_update_vnode_bitmap(actor_context.id) {
                        log_writer.update_vnode_bitmap(vnode_bitmap);
                    }
                    yield Message::Barrier(barrier);
                }
            }
        }
    }

    async fn execute_consume_log<S: Sink, R: LogReader>(
        sink: S,
        mut log_reader: R,
        columns: Vec<ColumnCatalog>,
        sink_metrics: SinkMetrics,
        sink_writer_param: SinkWriterParam,
    ) -> StreamExecutorResult<Message> {
        log_reader.init().await?;
        let mut sink_writer = sink.new_writer(sink_writer_param).await?;

        let visible_columns = columns
            .iter()
            .enumerate()
            .filter_map(|(idx, column)| (!column.is_hidden).then_some(idx))
            .collect_vec();

        #[derive(Debug)]
        enum LogConsumerState {
            /// Mark that the log consumer is not initialized yet
            Uninitialized,

            /// Mark that a new epoch has begun.
            EpochBegun { curr_epoch: u64 },

            /// Mark that the consumer has just received a barrier
            BarrierReceived { prev_epoch: u64 },
        }

        let mut state = LogConsumerState::Uninitialized;

        loop {
            let (epoch, item): (u64, LogStoreReadItem) = log_reader.next_item().await?;
            if let LogStoreReadItem::UpdateVnodeBitmap(_) = &item {
                match &state {
                    LogConsumerState::BarrierReceived { .. } => {}
                    _ => unreachable!(
                        "update vnode bitmap can be accepted only right after \
                    barrier, but current state is {:?}",
                        state
                    ),
                }
            }
            // begin_epoch when not previously began
            state = match state {
                LogConsumerState::Uninitialized => {
                    sink_writer.begin_epoch(epoch).await?;
                    LogConsumerState::EpochBegun { curr_epoch: epoch }
                }
                LogConsumerState::EpochBegun { curr_epoch } => {
                    assert!(
                        epoch >= curr_epoch,
                        "new epoch {} should not be below the current epoch {}",
                        epoch,
                        curr_epoch
                    );
                    LogConsumerState::EpochBegun { curr_epoch: epoch }
                }
                LogConsumerState::BarrierReceived { prev_epoch } => {
                    assert!(
                        epoch > prev_epoch,
                        "new epoch {} should be greater than prev epoch {}",
                        epoch,
                        prev_epoch
                    );
                    sink_writer.begin_epoch(epoch).await?;
                    LogConsumerState::EpochBegun { curr_epoch: epoch }
                }
            };
            match item {
                LogStoreReadItem::StreamChunk { chunk, .. } => {
                    let chunk = if visible_columns.len() != columns.len() {
                        // Do projection here because we may have columns that aren't visible to
                        // the downstream.
                        chunk.project(&visible_columns)
                    } else {
                        chunk
                    };
                    if let Err(e) = sink_writer.write_batch(chunk).await {
                        sink_writer.abort().await?;
                        return Err(e.into());
                    }
                }
                LogStoreReadItem::Barrier { is_checkpoint } => {
                    let prev_epoch = match state {
                        LogConsumerState::EpochBegun { curr_epoch } => curr_epoch,
                        _ => unreachable!("epoch must have begun before handling barrier"),
                    };
                    if is_checkpoint {
                        let start_time = Instant::now();
                        sink_writer.barrier(true).await?;
                        sink_metrics
                            .sink_commit_duration_metrics
                            .observe(start_time.elapsed().as_millis() as f64);
                        log_reader
                            .truncate(TruncateOffset::Barrier { epoch: prev_epoch })
                            .await?;
                    } else {
                        sink_writer.barrier(false).await?;
                    }
                    state = LogConsumerState::BarrierReceived { prev_epoch }
                }
                LogStoreReadItem::UpdateVnodeBitmap(vnode_bitmap) => {
                    sink_writer.update_vnode_bitmap(vnode_bitmap).await?;
                }
            }
        }
    }
}

impl<F: LogStoreFactory> Executor for SinkExecutor<F> {
    fn execute(self: Box<Self>) -> BoxedMessageStream {
        self.execute_inner()
    }

    fn schema(&self) -> &Schema {
        &self.input_schema
    }

    fn pk_indices(&self) -> super::PkIndicesRef<'_> {
        &self.pk_indices
    }

    fn identity(&self) -> &str {
        &self.identity
    }
}

#[cfg(test)]
mod test {
    use risingwave_common::catalog::{ColumnDesc, ColumnId};

    use super::*;
    use crate::common::log_store::in_mem::BoundedInMemLogStoreFactory;
    use crate::executor::test_utils::*;
    use crate::executor::ActorContext;

    #[tokio::test]
    async fn test_force_append_only_sink() {
        use risingwave_common::array::stream_chunk::StreamChunk;
        use risingwave_common::array::StreamChunkTestExt;
        use risingwave_common::types::DataType;

        use crate::executor::Barrier;

        let properties = maplit::hashmap! {
            "connector".into() => "blackhole".into(),
            "type".into() => "append-only".into(),
            "force_append_only".into() => "true".into()
        };

        // We have two visible columns and one hidden column. The hidden column will be pruned out
        // within the sink executor.
        let columns = vec![
            ColumnCatalog {
                column_desc: ColumnDesc::unnamed(ColumnId::new(0), DataType::Int64),
                is_hidden: false,
            },
            ColumnCatalog {
                column_desc: ColumnDesc::unnamed(ColumnId::new(1), DataType::Int64),
                is_hidden: false,
            },
            ColumnCatalog {
                column_desc: ColumnDesc::unnamed(ColumnId::new(2), DataType::Int64),
                is_hidden: true,
            },
        ];
        let schema: Schema = columns
            .iter()
            .map(|column| Field::from(column.column_desc.clone()))
            .collect();
        let pk = vec![0];

        let mock = MockSource::with_messages(
            schema,
            pk.clone(),
            vec![
                Message::Barrier(Barrier::new_test_barrier(1)),
                Message::Chunk(std::mem::take(&mut StreamChunk::from_pretty(
                    " I I I
                    + 3 2 1",
                ))),
                Message::Barrier(Barrier::new_test_barrier(2)),
                Message::Chunk(std::mem::take(&mut StreamChunk::from_pretty(
                    "  I I I
                    U- 3 2 1
                    U+ 3 4 1
                     + 5 6 7",
                ))),
                Message::Chunk(std::mem::take(&mut StreamChunk::from_pretty(
                    " I I I
                    - 5 6 7",
                ))),
            ],
        );

        let sink_param = SinkParam {
            sink_id: 0.into(),
            properties,
            columns: columns
                .iter()
                .filter(|col| !col.is_hidden)
                .map(|col| col.column_desc.clone())
                .collect(),
            downstream_pk: pk.clone(),
            sink_type: SinkType::ForceAppendOnly,
            db_name: "test".into(),
            sink_from_name: "test".into(),
        };

        let sink_executor = SinkExecutor::new(
            Box::new(mock),
            Arc::new(StreamingMetrics::unused()),
            SinkWriterParam::default(),
            sink_param,
            columns.clone(),
            ActorContext::create(0),
            BoundedInMemLogStoreFactory::new(1),
            pk,
        )
        .await
        .unwrap();

        let mut executor = SinkExecutor::execute(Box::new(sink_executor));

        // Barrier message.
        executor.next().await.unwrap().unwrap();

        let chunk_msg = executor.next().await.unwrap().unwrap();
        assert_eq!(
            chunk_msg.into_chunk().unwrap().compact(),
            StreamChunk::from_pretty(
                " I I I
                + 3 2 1",
            )
        );

        // Barrier message.
        executor.next().await.unwrap().unwrap();

        let chunk_msg = executor.next().await.unwrap().unwrap();
        assert_eq!(
            chunk_msg.into_chunk().unwrap().compact(),
            StreamChunk::from_pretty(
                " I I I
                + 3 4 1
                + 5 6 7",
            )
        );

        // Should not receive the third stream chunk message because the force-append-only sink
        // executor will drop all DELETE messages.

        // The last barrier message.
        executor.next().await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_empty_barrier_sink() {
        use risingwave_common::types::DataType;

        use crate::executor::Barrier;

        let properties = maplit::hashmap! {
            "connector".into() => "blackhole".into(),
            "type".into() => "append-only".into(),
            "force_append_only".into() => "true".into()
        };
        let columns = vec![
            ColumnCatalog {
                column_desc: ColumnDesc::unnamed(ColumnId::new(0), DataType::Int64),
                is_hidden: false,
            },
            ColumnCatalog {
                column_desc: ColumnDesc::unnamed(ColumnId::new(1), DataType::Int64),
                is_hidden: false,
            },
        ];
        let schema: Schema = columns
            .iter()
            .map(|column| Field::from(column.column_desc.clone()))
            .collect();
        let pk = vec![0];

        let mock = MockSource::with_messages(
            schema,
            pk.clone(),
            vec![
                Message::Barrier(Barrier::new_test_barrier(1)),
                Message::Barrier(Barrier::new_test_barrier(2)),
                Message::Barrier(Barrier::new_test_barrier(3)),
            ],
        );

        let sink_param = SinkParam {
            sink_id: 0.into(),
            properties,
            columns: columns
                .iter()
                .filter(|col| !col.is_hidden)
                .map(|col| col.column_desc.clone())
                .collect(),
            downstream_pk: pk.clone(),
            sink_type: SinkType::ForceAppendOnly,
            db_name: "test".into(),
            sink_from_name: "test".into(),
        };

        let sink_executor = SinkExecutor::new(
            Box::new(mock),
            Arc::new(StreamingMetrics::unused()),
            SinkWriterParam::default(),
            sink_param,
            columns,
            ActorContext::create(0),
            BoundedInMemLogStoreFactory::new(1),
            pk,
        )
        .await
        .unwrap();

        let mut executor = SinkExecutor::execute(Box::new(sink_executor));

        // Barrier message.
        assert_eq!(
            executor.next().await.unwrap().unwrap(),
            Message::Barrier(Barrier::new_test_barrier(1))
        );

        // Barrier message.
        assert_eq!(
            executor.next().await.unwrap().unwrap(),
            Message::Barrier(Barrier::new_test_barrier(2))
        );

        // The last barrier message.
        assert_eq!(
            executor.next().await.unwrap().unwrap(),
            Message::Barrier(Barrier::new_test_barrier(3))
        );
    }
}
