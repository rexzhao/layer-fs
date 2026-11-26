//! Selection of an inode-table branch record: whole, or a COPY/INSERT program
//! against its predecessor (#360 item 3).
//!
//! The candidate is the first advisory predecessor the construction carries (the
//! page this branch replaces). It is used only when it is a stored
//! `InodeBranch` whose chain is below the metadata depth cap and whose chain
//! work plus this branch stays within the metadata chain budgets. Exactly one
//! trial runs, with the pooled producer and its match budget; the program is
//! kept only when it is shorter than the whole record. Every refusal stores the
//! branch whole, which is what every Store before this form holds.

use layerfs_content::{ObjectId, ObjectRole};

use crate::encoding::delta::select::{note_depth, SelectInput};
use crate::encoding::full::EncodedRecord;
use crate::encoding::pool::delta;
use crate::error::StorageResult;
use crate::pack::layout::PackLane;
use crate::save::SaveProfile;
use std::time::Instant;

/// What branch selection did, apart from the payload counts (#360 item 3).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BranchCounters {
    /// Branches offered.
    pub branches: u64,
    /// Branches stored as a program against their predecessor.
    pub delta_branches: u64,
    /// Branches stored whole.
    pub full_branches: u64,
    /// Programs built.
    pub trials: u64,
    /// Predecessors refused at the depth cap.
    pub depth_capped: u64,
    /// Predecessors refused for the chain budgets.
    pub work_exceeded: u64,
    /// Branches stored, by chain depth (whole is 0).
    pub depths: super::select::DepthHistogram,
}

impl BranchCounters {
    /// Adds `other` to these counts.
    pub fn accumulate(&mut self, other: &Self) {
        for (total, value) in [
            (&mut self.branches, other.branches),
            (&mut self.delta_branches, other.delta_branches),
            (&mut self.full_branches, other.full_branches),
            (&mut self.trials, other.trials),
            (&mut self.depth_capped, other.depth_capped),
            (&mut self.work_exceeded, other.work_exceeded),
        ] {
            *total = total.saturating_add(value);
        }
        super::select::add_depths(&mut self.depths, &other.depths);
    }
}

/// Chooses the record of one `InodeBranch` whose whole record is `full`.
pub(super) fn select(
    input: &mut SelectInput<'_>,
    canonical: &[u8],
    advisory: &[ObjectId],
    full: EncodedRecord,
) -> StorageResult<EncodedRecord> {
    input.counters.branch.branches = input.counters.branch.branches.saturating_add(1);
    if let Some(record) = trial(input, canonical, advisory, &full)? {
        return Ok(record);
    }
    let branch = &mut input.counters.branch;
    branch.full_branches = branch.full_branches.saturating_add(1);
    note_depth(&mut branch.depths, 0);
    Ok(full)
}

fn trial(
    input: &mut SelectInput<'_>,
    canonical: &[u8],
    advisory: &[ObjectId],
    full: &EncodedRecord,
) -> StorageResult<Option<EncodedRecord>> {
    let Some(base_id) = advisory.first().copied() else {
        return Ok(None);
    };
    let Some(location) = input.connection.location(base_id, i64::MAX)? else {
        return Ok(None);
    };
    if location.role != ObjectRole::InodeBranch {
        return Ok(None);
    }
    let capacities = input.capacities;
    let started = Instant::now();
    let mut groups = crate::encoding::GroupCache::new();
    let base = input.pool.branch_canonical_with_caches(
        input.connection,
        capacities,
        i64::MAX,
        input.decode,
        location,
        Some(crate::encoding::pool::read::PooledCaches {
            encoded: input.packs,
            decoded: &mut groups,
        }),
    );
    SaveProfile::charge(&mut input.profile.resolve.acquire_ns, started);
    let (base, chain) = base?;
    let branch = &mut input.counters.branch;
    if chain.depth >= capacities.metadata_delta_max_depth {
        branch.depth_capped = branch.depth_capped.saturating_add(1);
        return Ok(None);
    }
    if chain.canonical.saturating_add(canonical.len() as u64)
        > capacities.metadata_chain_canonical_limit
        || chain.encoded.saturating_add(full.record.len() as u64)
            > capacities.metadata_chain_encoded_limit
    {
        branch.work_exceeded = branch.work_exceeded.saturating_add(1);
        return Ok(None);
    }
    branch.trials = branch.trials.saturating_add(1);
    let mut budget = crate::policy::METADATA_MATCH_BUDGET_BYTES;
    let started = Instant::now();
    let program = delta::build(base_id, &base, canonical, &mut budget);
    SaveProfile::charge(&mut input.profile.delta_ns, started);
    let Some(program) = program? else {
        return Ok(None);
    };
    if program.len() >= full.record.len() {
        return Ok(None);
    }
    branch.delta_branches = branch.delta_branches.saturating_add(1);
    note_depth(&mut branch.depths, chain.depth.saturating_add(1));
    Ok(Some(EncodedRecord {
        lane: PackLane::Ordinary,
        record: program,
        canonical_length: canonical.len(),
        raw_length: canonical.len(),
        base: Some(base_id),
    }))
}
