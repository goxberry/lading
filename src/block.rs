use std::{
    convert::TryInto,
    num::{NonZeroU32, NonZeroUsize},
};

use metrics::gauge;
use rand::{prelude::SliceRandom, Rng};

use crate::payload::Serialize;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Error {
    Chunk(ChunkError),
}

impl From<ChunkError> for Error {
    fn from(error: ChunkError) -> Self {
        Error::Chunk(error)
    }
}

#[derive(Debug)]
pub(crate) struct Block {
    pub(crate) total_bytes: NonZeroU32,
    pub(crate) lines: u64,
    pub(crate) bytes: Vec<u8>,
}

#[inline]
fn total_newlines(input: &[u8]) -> u64 {
    bytecount::count(input, b'\n') as u64
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ChunkError {
    /// The slice of byte sizes given to [`chunk_bytes`] was empty.
    EmptyBlockBytes,
    /// The `total_bytes` parameter is insufficient.
    InsufficientTotalBytes,
}

/// Construct a vec of block sizes that fit into `total_bytes`.
///
/// When calling [`construct_block_cache`] it's necessary to supply a
/// `block_chunks` argument, defining the block sizes that will be used when
/// serializing. Callers _generally_ will want to hit a certain total bytes
/// number of blocks and getting `total_bytes` parceled
/// up correctly is not necessarily straightforward. This utility method does
/// the computation in cases where it would otherwise be annoying. From the
/// allowable block sizes -- defined by `block_byte_sizes` -- a random member is
/// chosen and is deducted from the total bytes remaining. This process
/// continues until the total bytes remaining falls below the smallest block
/// size. It's possible that a user could supply just the right parameters to
/// make this loop infinitely. A more clever algorithm would be great.
///
/// # Errors
///
/// Function will return an error if `block_byte_sizes` is empty or if a member
/// of `block_byte_sizes` is large than `total_bytes`.
///
/// # Panics
///
/// Function will panic if `block_bytes_sizes` is empty.
pub(crate) fn chunk_bytes<R>(
    rng: &mut R,
    total_bytes: NonZeroUsize,
    block_byte_sizes: &[NonZeroUsize],
) -> Result<Vec<usize>, Error>
where
    R: Rng + Sized,
{
    if block_byte_sizes.is_empty() {
        return Err(ChunkError::EmptyBlockBytes.into());
    }
    for bb in block_byte_sizes {
        if *bb > total_bytes {
            return Err(ChunkError::InsufficientTotalBytes.into());
        }
    }

    let mut chunks = Vec::new();
    let mut bytes_remaining = total_bytes.get();
    let minimum = block_byte_sizes.iter().min().unwrap().get();
    let maximum = block_byte_sizes.iter().max().unwrap().get();

    while bytes_remaining > minimum {
        let bytes_max = std::cmp::min(maximum, bytes_remaining);
        let block_bytes = block_byte_sizes.choose(rng).unwrap().get();
        if block_bytes > bytes_max {
            continue;
        }
        chunks.push(block_bytes);
        bytes_remaining = bytes_remaining.saturating_sub(block_bytes);
    }
    Ok(chunks)
}

#[allow(clippy::ptr_arg)]
#[allow(clippy::cast_precision_loss)]
/// Construct a new block cache of form defined by `serializer`.
///
/// A "block cache" is a pre-made vec of serialized arbitrary instances of the
/// data implied by `serializer`. Considering that it's not cheap, necessarily,
/// to construct and serialize arbitrary data on the fly we want to do it ahead
/// of time. We vary the size of blocks -- via `block_chunks` -- to allow the
/// user to express a range of block sizes they wish to see.
///
/// # Panics
///
/// Function will panic if the `serializer` signals an error. In the futures we
/// would like to propagate this error to the caller.
pub(crate) fn construct_block_cache<R, S>(
    mut rng: R,
    serializer: &S,
    block_chunks: &[usize],
    labels: &Vec<(String, String)>,
) -> Vec<Block>
where
    S: Serialize,
    R: Rng,
{
    let mut block_cache: Vec<Block> = Vec::with_capacity(block_chunks.len());
    for block_size in block_chunks {
        let mut block: Vec<u8> = Vec::with_capacity(*block_size);
        serializer
            .to_bytes(&mut rng, *block_size, &mut block)
            .unwrap();
        block.shrink_to_fit();
        if block.is_empty() {
            // Blocks may be empty, especially when the amount of bytes
            // requested for the block are relatively low. This is a quirk of
            // our use of Arbitrary. We do not have the ability to tell that
            // library that we would like such and such number of bytes
            // approximately from an instance. This does mean that we sometimes
            // waste computation because the size of the block given cannot be
            // serialized into.
            continue;
        }
        let total_bytes = NonZeroU32::new(block.len().try_into().unwrap()).unwrap();
        let newlines = total_newlines(&block);
        block_cache.push(Block {
            total_bytes,
            lines: newlines,
            bytes: block,
        });
    }
    assert!(!block_cache.is_empty());
    gauge!("block_construction_complete", 1.0, labels);
    block_cache
}

#[cfg(test)]
mod test {
    use std::num::NonZeroUsize;

    use proptest::{collection, prelude::*};
    use rand::{rngs::SmallRng, SeedableRng};

    use crate::block::{chunk_bytes, ChunkError, Error};

    /// Construct our block_bytes_sizes vector and the total_bytes value. We are
    /// careful to never generate an empty vector nor a total_bytes that is less
    /// than any value in block_bytes_sizes.
    fn total_bytes_and_block_bytes() -> impl Strategy<Value = (NonZeroUsize, Vec<NonZeroUsize>)> {
        (1..usize::MAX).prop_flat_map(|total_bytes| {
            (
                Just(NonZeroUsize::new(total_bytes).unwrap()),
                collection::vec(
                    (1..total_bytes).prop_map(|i| NonZeroUsize::new(i).unwrap()),
                    1..1_000,
                ),
            )
        })
    }

    // No chunk is a returned set of chunks should ever be 0.
    proptest! {
        #[test]
        fn chunk_never_size_zero(seed: u64, (total_bytes, block_bytes_sizes) in total_bytes_and_block_bytes()) {
            let mut rng = SmallRng::seed_from_u64(seed);
            let chunks = chunk_bytes(&mut rng, total_bytes, &block_bytes_sizes).unwrap();

            for chunk in chunks {
                prop_assert!(chunk > 0);
            }
        }
    }

    // The vec of chunks must not be empty.
    proptest! {
        #[test]
        fn chunks_never_empty(seed: u64, (total_bytes, block_bytes_sizes) in total_bytes_and_block_bytes()) {
            let mut rng = SmallRng::seed_from_u64(seed);
            let chunks = chunk_bytes(&mut rng, total_bytes, &block_bytes_sizes).unwrap();
            prop_assert!(!chunks.is_empty())
        }
    }

    // Passing an empty block_byte_sizes always triggers an error condition.
    proptest! {
        #[test]
        fn chunks_empty_trigger_error(seed: u64, total_bytes in (1..usize::MAX).prop_map(|i| NonZeroUsize::new(i).unwrap())) {
            let mut rng = SmallRng::seed_from_u64(seed);
            prop_assert_eq!(Err(Error::Chunk(ChunkError::EmptyBlockBytes)), chunk_bytes(&mut rng, total_bytes, &[]));
        }
    }
}
