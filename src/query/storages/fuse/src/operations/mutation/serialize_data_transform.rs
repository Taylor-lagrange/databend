//  Copyright 2023 Datafuse Labs.
//
//  Licensed under the Apache License, Version 2.0 (the "License");
//  you may not use this file except in compliance with the License.
//  You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
//  Unless required by applicable law or agreed to in writing, software
//  distributed under the License is distributed on an "AS IS" BASIS,
//  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
//  See the License for the specific language governing permissions and
//  limitations under the License.

use std::any::Any;
use std::sync::Arc;

use common_datablocks::DataBlock;
use common_exception::ErrorCode;
use common_exception::Result;
use common_pipeline_core::processors::port::InputPort;
use common_storages_common::blocks_to_parquet;
use common_storages_table_meta::meta::BlockMeta;
use common_storages_table_meta::meta::ClusterStatistics;
use common_storages_table_meta::table::TableCompression;
use opendal::Operator;

use crate::io::write_data;
use crate::io::TableMetaLocationGenerator;
use crate::operations::mutation::Mutation;
use crate::operations::mutation::MutationTransformMeta;
use crate::operations::mutation::SerializeDataMeta;
use crate::operations::mutation::SerializeState;
use crate::operations::util;
use crate::operations::BloomIndexState;
use crate::pipelines::processors::port::OutputPort;
use crate::pipelines::processors::processor::Event;
use crate::pipelines::processors::Processor;
use crate::pruning::BlockIndex;
use crate::statistics::gen_columns_statistics;
use crate::statistics::ClusterStatsGenerator;

enum State {
    Consume,
    NeedSerialize(DataBlock),
    Serialized(SerializeState, Arc<BlockMeta>),
    Output(Mutation),
}

pub struct SerializeDataTransform {
    state: State,
    input: Arc<InputPort>,
    output: Arc<OutputPort>,
    output_data: Option<DataBlock>,

    location_gen: TableMetaLocationGenerator,
    dal: Operator,
    cluster_stats_gen: ClusterStatsGenerator,

    index: BlockIndex,
    origin_stats: Option<ClusterStatistics>,
    table_compression: TableCompression,
}

#[async_trait::async_trait]
impl Processor for SerializeDataTransform {
    fn name(&self) -> String {
        "SerializeDataTransform".to_string()
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }

    fn event(&mut self) -> Result<Event> {
        if matches!(self.state, State::NeedSerialize(_) | State::Output(_)) {
            return Ok(Event::Sync);
        }

        if matches!(self.state, State::Serialized(_, _)) {
            return Ok(Event::Async);
        }

        if self.output.is_finished() {
            return Ok(Event::Finished);
        }

        if !self.output.can_push() {
            return Ok(Event::NeedConsume);
        }

        if let Some(data_block) = self.output_data.take() {
            self.output.push_data(Ok(data_block));
            return Ok(Event::NeedConsume);
        }

        if self.input.is_finished() {
            self.output.finish();
            return Ok(Event::Finished);
        }

        if !self.input.has_data() {
            self.input.set_need_data();
            return Ok(Event::NeedData);
        }

        let mut input_data = self.input.pull_data().unwrap()?;
        let meta = input_data.take_meta();
        if meta.is_none() {
            self.state = State::Output(Mutation::DoNothing);
        } else {
            let meta = meta.unwrap();
            let meta = SerializeDataMeta::from_meta(&meta)?;
            self.index = meta.index;
            self.origin_stats = meta.cluster_stats.clone();
            if input_data.is_empty() {
                self.state = State::Output(Mutation::Deleted);
            } else {
                self.state = State::NeedSerialize(input_data);
            }
        }
        Ok(Event::Sync)
    }

    fn process(&mut self) -> Result<()> {
        match std::mem::replace(&mut self.state, State::Consume) {
            State::NeedSerialize(block) => {
                let cluster_stats = self
                    .cluster_stats_gen
                    .gen_with_origin_stats(&block, std::mem::take(&mut self.origin_stats))?;

                let row_count = block.num_rows() as u64;
                let block_size = block.memory_size() as u64;
                let (block_location, block_id) = self.location_gen.gen_block_location();

                // build block index.
                let location = self.location_gen.block_bloom_index_location(&block_id);
                let (bloom_index_state, column_distinct_count) =
                    BloomIndexState::try_create(&block, location)?;
                let col_stats = gen_columns_statistics(&block, Some(column_distinct_count))?;

                // serialize data block.
                let mut block_data = Vec::with_capacity(100 * 1024 * 1024);
                let schema = block.schema().clone();
                let (file_size, meta_data) = blocks_to_parquet(
                    &schema,
                    vec![block],
                    &mut block_data,
                    self.table_compression,
                )?;
                let col_metas = util::column_metas(&meta_data)?;

                // new block meta.
                let new_meta = Arc::new(BlockMeta::new(
                    row_count,
                    block_size,
                    file_size,
                    col_stats,
                    col_metas,
                    cluster_stats,
                    block_location.clone(),
                    Some(bloom_index_state.location.clone()),
                    bloom_index_state.size,
                    self.table_compression.into(),
                ));

                self.state = State::Serialized(
                    SerializeState {
                        block_data,
                        block_location: block_location.0,
                        index_data: bloom_index_state.data,
                        index_location: bloom_index_state.location.0,
                    },
                    new_meta,
                );
            }
            State::Output(op) => {
                let meta = MutationTransformMeta::create(self.index, op);
                self.output_data = Some(DataBlock::empty_with_meta(meta));
            }
            _ => return Err(ErrorCode::Internal("It's a bug.")),
        }
        Ok(())
    }

    async fn async_process(&mut self) -> Result<()> {
        match std::mem::replace(&mut self.state, State::Consume) {
            State::Serialized(serialize_state, block_meta) => {
                // write block data.
                write_data(
                    &serialize_state.block_data,
                    &self.dal,
                    &serialize_state.block_location,
                )
                .await?;
                // write index data.
                write_data(
                    &serialize_state.index_data,
                    &self.dal,
                    &serialize_state.index_location,
                )
                .await?;
                self.state = State::Output(Mutation::Replaced(block_meta));
            }
            _ => return Err(ErrorCode::Internal("It's a bug.")),
        }
        Ok(())
    }
}
