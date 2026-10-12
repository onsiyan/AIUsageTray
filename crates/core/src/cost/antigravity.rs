//! Reads token usage from Antigravity's conversation databases.
//!
//! Antigravity keeps each conversation in its own SQLite file
//! (`~/.gemini/antigravity/conversations/<id>.db`). The `gen_metadata` table
//! holds one protobuf record per model call, numbered in order, with the
//! model and its token counts; the call's time is the creation time of the
//! step it ended on, in the `steps` table. The format is Antigravity's own
//! and undocumented, so anything that does not decode as expected is
//! skipped rather than guessed.
//!
//! Only calls numbered after the last one read are fetched, so a scan reads
//! just what a conversation added since the previous one.

use std::collections::HashMap;
use std::path::Path;

use chrono::{DateTime, Local, NaiveDate};
use rusqlite::{Connection, OpenFlags, params};

use super::TokenCounts;

/// Gemini's higher long-context rates apply past this much input.
const LONG_CONTEXT_TOKENS: u64 = 200_000;

/// One model call's usage.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Generation {
    pub index: i64,
    pub day: Option<NaiveDate>,
    pub model: String,
    pub tokens: TokenCounts,
    pub long_context: bool,
}

/// The calls numbered `from` and later, in order.
pub(super) fn read_generations(path: &Path, from: i64) -> rusqlite::Result<Vec<Generation>> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(std::time::Duration::from_secs(2))?;
    let mut statement =
        connection.prepare("SELECT idx, data FROM gen_metadata WHERE idx >= ?1 ORDER BY idx")?;
    let records = statement
        .query_map(params![from], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut step_times: HashMap<i64, Option<i64>> = HashMap::new();
    let mut step_statement = connection.prepare("SELECT metadata FROM steps WHERE idx = ?1")?;
    let mut generations = Vec::with_capacity(records.len());
    for (index, data) in records {
        let Some(call) = decode_call(&data) else {
            // Kept as read, so a record that never decodes is not retried.
            generations.push(Generation {
                index,
                day: None,
                model: String::new(),
                tokens: TokenCounts::default(),
                long_context: false,
            });
            continue;
        };
        let seconds = call.last_step.and_then(|step| {
            *step_times.entry(step).or_insert_with(|| {
                step_statement
                    .query_row(params![step], |row| row.get::<_, Option<Vec<u8>>>(0))
                    .ok()
                    .flatten()
                    .and_then(|metadata| step_created_seconds(&metadata))
            })
        });
        let day = seconds
            .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
            .map(|time| time.with_timezone(&Local).date_naive());
        generations.push(Generation {
            index,
            day,
            long_context: call.tokens.input + call.tokens.cache_read > LONG_CONTEXT_TOKENS,
            model: call.model,
            tokens: call.tokens,
        });
    }
    Ok(generations)
}

#[derive(Debug, PartialEq)]
struct Call {
    model: String,
    tokens: TokenCounts,
    last_step: Option<i64>,
}

/// The model, tokens, and last step of one `gen_metadata` record.
///
/// The record's field 1 holds the call: field 19 the model, field 4 the
/// usage (2 input, 5 cache reads, 3 output, which is 9 thinking plus 10
/// reply), and field 20 string pairs, among them `last_step_index`.
fn decode_call(data: &[u8]) -> Option<Call> {
    let call = field_bytes(data, 1)?;
    let model = field_bytes(call, 19)
        .and_then(|model| std::str::from_utf8(model).ok())
        .map(str::trim)
        .filter(|model| !model.is_empty())?
        .to_owned();
    let usage = field_bytes(call, 4)?;
    let number = |field| field_varint(usage, field).unwrap_or(0);
    let output = number(3);
    let tokens = TokenCounts {
        input: number(2),
        cache_read: number(5),
        cache_write: 0,
        cache_write_1h: 0,
        output,
        reasoning: number(9).min(output),
    };
    let mut last_step = None;
    for (field, value) in Fields::new(call) {
        if field != 20 {
            continue;
        }
        let Value::Bytes(pair) = value else {
            continue;
        };
        if field_bytes(pair, 1) == Some(b"last_step_index".as_slice()) {
            last_step = field_bytes(pair, 2)
                .and_then(|value| std::str::from_utf8(value).ok())
                .and_then(|value| value.trim().parse().ok());
        }
    }
    Some(Call {
        model,
        tokens,
        last_step,
    })
}

/// When a step was created: field 1 of the step metadata is a timestamp
/// whose field 1 is seconds since 1970.
fn step_created_seconds(metadata: &[u8]) -> Option<i64> {
    let created = field_bytes(metadata, 1)?;
    let seconds = i64::try_from(field_varint(created, 1)?).ok()?;
    // Anything outside 2020 to 2100 is not a timestamp.
    (1_577_836_800..4_102_444_800)
        .contains(&seconds)
        .then_some(seconds)
}

#[derive(Debug, Clone, Copy)]
enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed,
}

/// The fields of a protobuf message, in order, stopping at the first
/// malformed one.
struct Fields<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Fields<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }

    fn varint(&mut self) -> Option<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *self.data.get(self.at)?;
            self.at += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte < 0x80 {
                return Some(value);
            }
        }
        None
    }

    fn skip(&mut self, length: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(length)?;
        let bytes = self.data.get(self.at..end)?;
        self.at = end;
        Some(bytes)
    }
}

impl<'a> Iterator for Fields<'a> {
    type Item = (u64, Value<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.at >= self.data.len() {
            return None;
        }
        let key = self.varint()?;
        let field = key >> 3;
        let value = match key & 7 {
            0 => Value::Varint(self.varint()?),
            1 => {
                self.skip(8)?;
                Value::Fixed
            }
            2 => {
                let length = usize::try_from(self.varint()?).ok()?;
                Value::Bytes(self.skip(length)?)
            }
            5 => {
                self.skip(4)?;
                Value::Fixed
            }
            _ => {
                self.at = self.data.len();
                return None;
            }
        };
        Some((field, value))
    }
}

fn field_bytes(data: &[u8], wanted: u64) -> Option<&[u8]> {
    Fields::new(data).find_map(|(field, value)| match value {
        Value::Bytes(bytes) if field == wanted => Some(bytes),
        _ => None,
    })
}

fn field_varint(data: &[u8], wanted: u64) -> Option<u64> {
    Fields::new(data).find_map(|(field, value)| match value {
        Value::Varint(number) if field == wanted => Some(number),
        _ => None,
    })
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    fn varint(mut value: u64, out: &mut Vec<u8>) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }

    fn number(field: u64, value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        varint(field << 3, &mut out);
        varint(value, &mut out);
        out
    }

    fn bytes(field: u64, value: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        varint((field << 3) | 2, &mut out);
        varint(value.len() as u64, &mut out);
        out.extend_from_slice(value);
        out
    }

    /// A `gen_metadata` record as Antigravity writes it, with the fields
    /// this reader uses and some it skips.
    pub(in crate::cost) fn call_record(
        model: &str,
        input: u64,
        cache: u64,
        thinking: u64,
        reply: u64,
        last_step: i64,
    ) -> Vec<u8> {
        let usage = [
            number(1, 1318),
            number(2, input),
            number(3, thinking + reply),
            number(5, cache),
            number(6, 24),
            number(9, thinking),
            number(10, reply),
        ]
        .concat();
        let pair = |key: &str, value: &str| {
            bytes(
                20,
                &[bytes(1, key.as_bytes()), bytes(2, value.as_bytes())].concat(),
            )
        };
        let call = [
            number(3, 1318),
            bytes(4, &usage),
            bytes(19, model.as_bytes()),
            pair("model_enum", "MODEL_PLACEHOLDER"),
            pair("last_step_index", &last_step.to_string()),
        ]
        .concat();
        [bytes(1, &call), bytes(4, b"c202a66c")].concat()
    }

    pub(in crate::cost) fn step_record(seconds: u64) -> Vec<u8> {
        [
            bytes(1, &[number(1, seconds), number(2, 5)].concat()),
            number(3, 1),
        ]
        .concat()
    }

    #[test]
    fn a_call_record_gives_its_model_tokens_and_step() {
        let call = decode_call(&call_record("gemini-3.8-flash", 3503, 16300, 125, 61, 7))
            .expect("decodes");
        assert_eq!(call.model, "gemini-3.8-flash");
        assert_eq!(call.last_step, Some(7));
        assert_eq!(
            call.tokens,
            TokenCounts {
                input: 3503,
                cache_read: 16300,
                cache_write: 0,
                cache_write_1h: 0,
                output: 186,
                reasoning: 125,
            }
        );
        assert_eq!(
            step_created_seconds(&step_record(1_787_821_152)),
            Some(1_787_821_152)
        );
        assert_eq!(step_created_seconds(&step_record(12)), None);
    }

    #[test]
    fn damaged_records_decode_to_nothing() {
        assert_eq!(decode_call(&[]), None);
        assert_eq!(decode_call(&[0x0a, 0xff, 0x01]), None);
        let mut cut = call_record("gemini-3.8-flash", 1, 2, 3, 4, 5);
        cut.truncate(cut.len() / 2);
        assert_eq!(decode_call(&cut), None);
    }
}
