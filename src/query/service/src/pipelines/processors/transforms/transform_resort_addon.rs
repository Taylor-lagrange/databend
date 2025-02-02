// Copyright 2021 Datafuse Labs
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

use common_exception::Result;
use common_expression::DataBlock;
use common_expression::DataSchemaRef;
use common_sql::evaluator::CompoundBlockOperator;
use common_storages_factory::Table;

use super::transform_resort_addon_without_source_schema::build_expression_transform;
use crate::pipelines::processors::port::InputPort;
use crate::pipelines::processors::port::OutputPort;
use crate::pipelines::processors::processor::ProcessorPtr;
use crate::pipelines::processors::transforms::transform::Transform;
use crate::pipelines::processors::transforms::transform::Transformer;
use crate::sessions::QueryContext;

pub struct TransformResortAddOn {
    expression_transform: CompoundBlockOperator,
    input_len: usize,
}

impl TransformResortAddOn
where Self: Transform
{
    pub fn try_create(
        ctx: Arc<QueryContext>,
        input: Arc<InputPort>,
        output: Arc<OutputPort>,
        input_schema: DataSchemaRef,
        output_schema: DataSchemaRef,
        table: Arc<dyn Table>,
    ) -> Result<ProcessorPtr> {
        let expression_transform =
            build_expression_transform(input_schema.clone(), output_schema, table, ctx)?;
        Ok(ProcessorPtr::create(Transformer::create(
            input,
            output,
            Self {
                expression_transform,
                input_len: input_schema.num_fields(),
            },
        )))
    }
}

impl Transform for TransformResortAddOn {
    const NAME: &'static str = "AddOnTransform";

    fn transform(&mut self, mut block: DataBlock) -> Result<DataBlock> {
        block = self.expression_transform.transform(block)?;
        let columns = block.columns()[self.input_len..].to_owned();
        Ok(DataBlock::new(columns, block.num_rows()))
    }
}
