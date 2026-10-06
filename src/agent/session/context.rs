//! What the model sees: the conversation projection over the records.
//!
//! The file holds the whole history; the model is sent a *window* of it. Everything in this
//! module answers "what would be sent", and the answer changes when a checkpoint is written:
//! a `compacted` or `pruned` record stands in for the span of conversation before it, so the
//! records it replaces stay on disk but stop being part of the projection.
//!
//! Kept apart from the file handling in [`super`] because the two are read differently: this
//! is arithmetic over records (no IO, no locks), while the parent is about appending,
//! locking and recovering bytes.

use crate::llm::Message;

use super::{ContextSnapshot, Record, Session};

impl Session {
    pub fn file_operations(&self) -> crate::agent::compact::FileOps {
        crate::agent::compact::FileOps::collect(self.records.iter().filter_map(Record::message))
    }

    /// Keep short user corrections verbatim across repeated summaries, within a fixed budget.
    pub fn checkpoint_facts(
        &self,
        max_request_chars: usize,
    ) -> crate::agent::compact::CheckpointFacts {
        let mut remaining = max_request_chars;
        let mut requests = Vec::new();
        for message in self.records.iter().rev().filter_map(Record::message) {
            if !matches!(message, Message::User { .. })
                || crate::agent::r#loop::is_environment_block(message)
            {
                continue;
            }
            let text = message.text();
            let chars = text.chars().count();
            if chars == 0 || chars > remaining {
                continue;
            }
            requests.push(text);
            remaining -= chars;
            if remaining == 0 || requests.len() == 8 {
                break;
            }
        }
        requests.reverse();
        crate::agent::compact::CheckpointFacts {
            files: self.file_operations(),
            user_requests: requests,
        }
    }

    /// The conversation as the model should see it: everything after the last
    /// checkpoint, using the checkpoint's replacement history in its place.
    fn context_entries(&self) -> impl Iterator<Item = (&Message, &str)> {
        let checkpoint = self.last_checkpoint_index();
        let replacement = checkpoint
            .map(|index| match &self.records[index] {
                Record::Compacted {
                    replacement_history,
                    replacement_ids,
                    ..
                }
                | Record::Pruned {
                    replacement_history,
                    replacement_ids,
                    ..
                } => (replacement_history, replacement_ids),
                _ => unreachable!(),
            })
            .into_iter()
            .flat_map(|(messages, ids)| messages.iter().zip(ids.iter().map(String::as_str)));
        let start = checkpoint.map_or(0, |index| index + 1);
        replacement.chain(
            self.records[start..]
                .iter()
                .filter_map(|record| record.message().map(|message| (message, record.id()))),
        )
    }

    pub fn context_snapshot(&self) -> ContextSnapshot {
        let (messages, entry_ids) = self
            .context_entries()
            .map(|(message, id)| (message.clone(), id.to_string()))
            .unzip();
        ContextSnapshot {
            messages,
            entry_ids,
        }
    }

    pub fn context_messages(&self) -> Vec<Message> {
        self.context_entries()
            .map(|(message, _)| message.clone())
            .collect()
    }

    /// Latest observed prompt+answer plus messages appended since that observation.
    pub fn measured_context_tokens(&self) -> Option<u64> {
        let usage = self.last_usage?;
        if usage
            .input
            .saturating_add(usage.cache_read)
            .saturating_add(usage.cache_write)
            == 0
        {
            return None;
        }
        let index = self.last_usage_index?;
        let appended = self.records[index + 1..]
            .iter()
            .filter_map(Record::message)
            .map(Message::estimate_tokens)
            .sum::<u64>();
        Some(
            usage
                .input
                .saturating_add(usage.output)
                .saturating_add(usage.cache_read)
                .saturating_add(usage.cache_write)
                .saturating_add(appended),
        )
    }

    /// Whether the model would see nothing but the environment block.
    ///
    /// True for a session that has not been spoken to: the block is the head every session
    /// starts with, and until something else is in the context there is no conversation to
    /// measure — only the fixed prefix every request carries, tool schemas included. The
    /// footer reads this to say "nothing spent yet" instead of printing that prefix as if
    /// the user had spent it.
    pub fn context_holds_only_environment(&self) -> bool {
        self.context_entries()
            .all(|(message, _)| crate::agent::r#loop::is_environment_block(message))
    }

    pub fn last_checkpoint_index(&self) -> Option<usize> {
        self.records
            .iter()
            .rposition(|record| matches!(record, Record::Compacted { .. } | Record::Pruned { .. }))
    }
}
