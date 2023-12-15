use std::{collections::VecDeque, rc::Rc};

use tokenizers::Encoding;

use crate::{
    openai::{responses::APIError, utils::get_created_time_secs},
    paged_attention::input_metadata::InputMetadata,
    scheduler::{
        cache_engine::{CacheConfig, CacheEngine},
        scheduler::{Scheduler, SchedulerConfig},
        sequence::{Sequence, SequenceGroup},
    },
};

use super::{ModulePipeline, _make_tensor_with_pad};

use candle_core::{Device, Tensor};

struct PreparedInputs {
    tokens: Tensor,
    positions: Tensor,
    metadata: InputMetadata,
}

const _PAD_SLOT_ID: i64 = -1;

pub struct LLMEngine<'a> {
    pipeline: Box<dyn ModulePipeline<'a>>,
    scheduler: Scheduler,
    seq_id: usize,
    cache_config: CacheConfig,
    group_id: usize,
    cache_engine: CacheEngine,
    sliding_window: Option<usize>,
}

impl<'a> LLMEngine<'a> {
    pub fn new(
        pipeline: Box<dyn ModulePipeline<'a>>,
        scheduler_config: SchedulerConfig,
        cache_config: CacheConfig,
    ) -> Result<Self, APIError> {
        Ok(Self {
            pipeline,
            scheduler: Scheduler::new(scheduler_config, &cache_config),
            seq_id: 0,
            cache_config,
            group_id: 0,
            cache_engine: CacheEngine::new(
                pipeline.get_model_config(),
                cache_config,
                pipeline.get_dtype(),
            )?,
            sliding_window: pipeline.get_model_config().get_sliding_window(),
        })
    }

    fn add_request(&mut self, prompt: Encoding) {
        let seq = Rc::new(Sequence::new(
            prompt
                .get_ids()
                .to_vec()
                .iter()
                .map(|x| *x as usize)
                .collect::<Vec<_>>(),
            self.seq_id,
            self.cache_config.block_size,
        ));
        self.seq_id += 1;
        let seq_group = SequenceGroup::new(&vec![seq], get_created_time_secs(), self.group_id);
        self.group_id += 1;

        self.scheduler.add_sequence(seq_group);
    }

    pub fn generate(&mut self, prompt: Encoding) -> Result<(), APIError> {
        self.add_request(prompt);
        while self.scheduler.has_unfinished_sequences() {
            let scheduler_outputs = self.scheduler.schedule();
            if !scheduler_outputs.ignored_seq_groups.is_empty() {
                todo!();
            }
            let scheduled = &*scheduler_outputs.scheduled;

            let PreparedInputs {
                tokens,
                positions,
                metadata,
            } = if scheduled
                .front()
                .unwrap()
                .get_seqs()
                .values()
                .nth(0)
                .unwrap()
                .is_prompt()
            {
                self.prepare_prompt(&*scheduled)
            } else {
                // Because of the KV cache, we only need to take
                // the last token.
                self.prepare_decode(&*scheduled)
            }?;
        }
        todo!()
    }

    fn prepare_prompt(
        &self,
        groups: &VecDeque<Rc<SequenceGroup>>,
    ) -> Result<PreparedInputs, APIError> {
        let mut prompt_lens = Vec::new();
        let mut input_tokens = Vec::new();
        let mut input_positions = Vec::new();
        let mut slot_mappings = Vec::new();
        for group in groups {
            for seq in group.get_seqs().values() {
                let prompt_ids = seq.get_token_ids();

                let prompt_len = prompt_ids.len();
                prompt_lens.push(prompt_len);

                input_tokens.push(prompt_ids);
                input_positions.push((0..prompt_len).collect::<Vec<_>>());
                let table = self.scheduler.block_engine.block_tables.get(&seq.get_id());
                if table == None {
                    // Will be None during profiling.
                    slot_mappings.push(vec![_PAD_SLOT_ID].repeat(prompt_len));
                    continue;
                }
                let mut table = table
                    .unwrap()
                    .iter()
                    .map(|block| block.block_id)
                    .collect::<Vec<_>>();

                let start_idx = if let Some(sliding_window) = self.sliding_window {
                    0.min(prompt_len - sliding_window)
                } else {
                    0
                };

                let mut slot_mapping = Vec::new();
                for i in 0..prompt_len {
                    if i < start_idx {
                        // Pad [0,start_idx) with _PAD_TOKEN_ID
                        slot_mapping.push(_PAD_SLOT_ID);
                    }

                    let block_number = table.get(i / self.cache_config.block_size).unwrap();
                    let block_offset = i % self.cache_config.block_size;
                    let slot = block_number * self.cache_config.block_size + block_offset;
                    slot_mapping.push(slot.try_into().unwrap());
                }
                slot_mappings.push(slot_mapping);
            }
        }

        let max_prompt_len = prompt_lens.iter().max().unwrap();
        let input_tokens = _make_tensor_with_pad(
            input_tokens
                .iter()
                .map(|x| x.iter().map(|x| *x as i64).collect::<Vec<_>>())
                .collect::<Vec<_>>(),
            *max_prompt_len,
            0,
        )?;
        let input_positions = _make_tensor_with_pad(
            input_positions
                .iter()
                .map(|x| x.iter().map(|x| *x as i64).collect::<Vec<_>>())
                .collect::<Vec<_>>(),
            *max_prompt_len,
            0,
        )?;
        let slot_mapping = _make_tensor_with_pad(slot_mappings, *max_prompt_len, _PAD_SLOT_ID)?;

        Ok(PreparedInputs {
            tokens: input_tokens,
            positions: input_positions,
            metadata: InputMetadata {
                prompt_lens,
                slot_mapping,
                max_context_len: None,
                context_lens: None,
                block_tables: None,
                attn_bias: None,
                is_prompt: true,
            },
        })
    }

    fn prepare_decode(
        &self,
        groups: &VecDeque<Rc<SequenceGroup>>,
    ) -> Result<PreparedInputs, APIError> {
        let mut input_tokens = Vec::new();
        let mut input_positions = Vec::new();
        let mut context_lens = Vec::new();
        let mut slot_mappings = Vec::new();
        let mut block_tables = Vec::new();
        for group in groups {
            for (_, seq) in group.get_seqs() {
                let last_token_id = seq.get_last_token_id();
                input_tokens.push(vec![last_token_id]);

                let position = seq.get_len() - 1;
                input_positions.push(vec![position]);

                let context_len = if let Some(sliding_window) = self.sliding_window {
                    seq.get_len().min(sliding_window)
                } else {
                    seq.get_len()
                };
                context_lens.push(context_len);

                let table = self
                    .scheduler
                    .block_engine
                    .block_tables
                    .get(&seq.get_id())
                    .unwrap();
                let mut table = table.iter().map(|block| block.block_id).collect::<Vec<_>>();

                let block_number = table.get(position / self.cache_config.block_size).unwrap();
                let block_offset = position % self.cache_config.block_size;
                let slot = block_number * self.cache_config.block_size + block_offset;
                let slot = slot.try_into().unwrap();
                slot_mappings.push(vec![slot]);

                if let Some(sliding_window) = self.sliding_window {
                    let sliding_window_blocks = sliding_window / self.cache_config.block_size;
                    block_tables.push(
                        table
                            .get(table.len() - sliding_window_blocks..)
                            .unwrap()
                            .to_vec(),
                    );
                } else {
                    block_tables.push(table);
                }
            }
        }

        let input_tokens = _make_tensor_with_pad(
            input_tokens
                .iter()
                .map(|x| x.iter().map(|x| *x as i64).collect::<Vec<_>>())
                .collect::<Vec<_>>(),
            1,
            0,
        )?;
        let input_positions = _make_tensor_with_pad(
            input_positions
                .iter()
                .map(|x| x.iter().map(|x| *x as i64).collect::<Vec<_>>())
                .collect::<Vec<_>>(),
            1,
            0,
        )?;
        let slot_mapping = _make_tensor_with_pad(slot_mappings, 1, _PAD_SLOT_ID)?;

        let max_context_len = context_lens.iter().max().unwrap();
        let context_lens = Tensor::from_vec(
            context_lens.iter().map(|x| *x as i64).collect::<Vec<_>>(),
            (context_lens.len(),),
            &Device::new_cuda(0).map_err(APIError::from)?,
        )
        .map_err(APIError::from)?;

        let max_block_table_len = block_tables.iter().map(|x| x.len()).max().unwrap();
        let block_tables = _make_tensor_with_pad(
            block_tables
                .iter()
                .map(|x| x.iter().map(|x| *x as i64).collect::<Vec<_>>())
                .collect::<Vec<_>>(),
            max_block_table_len,
            0,
        )?;

        Ok(PreparedInputs {
            tokens: input_tokens,
            positions: input_positions,
            metadata: InputMetadata {
                prompt_lens: vec![],
                slot_mapping,
                max_context_len: Some(*max_context_len),
                context_lens: Some(context_lens),
                block_tables: Some(block_tables),
                attn_bias: None,
                is_prompt: false,
            },
        })
    }
}
