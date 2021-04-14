/*
 * The following license applies to this file, which has been largely
 * derived from the files `js/src/jit/BacktrackingAllocator.h` and
 * `js/src/jit/BacktrackingAllocator.cpp` in Mozilla Firefox:
 *
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/.
 */

//! Backtracking register allocator on SSA code ported from IonMonkey's
//! BacktrackingAllocator.

/*
 * TODO:
 *
 * - tune heuristics:
 *   - splits:
 *     - safepoints?
 *     - split just before uses with fixed regs and/or just after defs
 *       with fixed regs?
 *   - try-any-reg allocate loop should randomly probe in caller-save
 *     ("preferred") regs first -- have a notion of "preferred regs" in
 *     MachineEnv?
 *   - measure average liverange length / number of splits / ...
 *
 * - reused-input reg: don't allocate register for input that is reused.
 *
 * - more fuzzing:
 *   - test with *multiple* fixed-reg constraints on one vreg (same
 *     inst, different insts)
 *
 * - modify CL to generate SSA VCode
 *   - lower blockparams to blockparams directly
 *   - use temps properly (`alloc_tmp()` vs `alloc_reg()`)
 *
 * - produce stackmaps
 *   - stack constraint (also: unify this with stack-args? spillslot vs user stackslot?)
 *   - vreg reffyness
 *   - if reffy vreg, add to stackmap lists during reification scan
 */

#![allow(dead_code, unused_imports)]

use crate::bitvec::BitVec;
use crate::cfg::CFGInfo;
use crate::index::ContainerComparator;
use crate::moves::ParallelMoves;
use crate::{
    define_index, domtree, Allocation, AllocationKind, Block, Edit, Function, Inst, InstPosition,
    MachineEnv, Operand, OperandKind, OperandPolicy, OperandPos, Output, PReg, ProgPoint,
    RegAllocError, RegClass, SpillSlot, VReg,
};
use log::debug;
use smallvec::{smallvec, SmallVec};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BinaryHeap};
use std::fmt::Debug;

#[cfg(not(debug))]
fn validate_ssa<F: Function>(_: &F, _: &CFGInfo) -> Result<(), RegAllocError> {
    Ok(())
}

#[cfg(debug)]
fn validate_ssa<F: Function>(f: &F, cfginfo: &CFGInfo) -> Result<(), RegAllocError> {
    crate::validate_ssa(f, cfginfo)
}

/// A range from `from` (inclusive) to `to` (exclusive).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodeRange {
    from: ProgPoint,
    to: ProgPoint,
}

impl CodeRange {
    pub fn is_empty(&self) -> bool {
        self.from == self.to
    }
    pub fn contains(&self, other: &Self) -> bool {
        other.from >= self.from && other.to <= self.to
    }
    pub fn contains_point(&self, other: ProgPoint) -> bool {
        other >= self.from && other < self.to
    }
    pub fn overlaps(&self, other: &Self) -> bool {
        other.to > self.from && other.from < self.to
    }
    pub fn len(&self) -> usize {
        self.to.inst.index() - self.from.inst.index()
    }
}

impl std::cmp::PartialOrd for CodeRange {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl std::cmp::Ord for CodeRange {
    fn cmp(&self, other: &Self) -> Ordering {
        if self.to <= other.from {
            Ordering::Less
        } else if self.from >= other.to {
            Ordering::Greater
        } else {
            Ordering::Equal
        }
    }
}

define_index!(LiveBundleIndex);
define_index!(LiveRangeIndex);
define_index!(SpillSetIndex);
define_index!(UseIndex);
define_index!(DefIndex);
define_index!(VRegIndex);
define_index!(PRegIndex);
define_index!(SpillSlotIndex);

type LiveBundleVec = SmallVec<[LiveBundleIndex; 4]>;

#[derive(Clone, Debug)]
struct LiveRange {
    range: CodeRange,
    vreg: VRegIndex,
    bundle: LiveBundleIndex,
    uses_spill_weight: u32,
    num_fixed_uses_and_flags: u32,

    first_use: UseIndex,
    last_use: UseIndex,
    def: DefIndex,

    next_in_bundle: LiveRangeIndex,
    next_in_reg: LiveRangeIndex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
enum LiveRangeFlag {
    Minimal = 1,
    Fixed = 2,
}

impl LiveRange {
    #[inline(always)]
    pub fn num_fixed_uses(&self) -> u32 {
        self.num_fixed_uses_and_flags & ((1 << 24) - 1)
    }
    #[inline(always)]
    pub fn set_num_fixed_uses(&mut self, count: u32) {
        debug_assert!(count < (1 << 24));
        self.num_fixed_uses_and_flags = (self.num_fixed_uses_and_flags & !((1 << 24) - 1)) | count;
    }
    #[inline(always)]
    pub fn inc_num_fixed_uses(&mut self) {
        debug_assert!(self.num_fixed_uses_and_flags & ((1 << 24) - 1) < ((1 << 24) - 1));
        self.num_fixed_uses_and_flags += 1;
    }
    #[inline(always)]
    pub fn dec_num_fixed_uses(&mut self) {
        debug_assert!(self.num_fixed_uses_and_flags & ((1 << 24) - 1) > 0);
        self.num_fixed_uses_and_flags -= 1;
    }
    #[inline(always)]
    pub fn set_flag(&mut self, flag: LiveRangeFlag) {
        self.num_fixed_uses_and_flags |= (flag as u32) << 24;
    }
    #[inline(always)]
    pub fn clear_flag(&mut self, flag: LiveRangeFlag) {
        self.num_fixed_uses_and_flags &= !((flag as u32) << 24);
    }
    #[inline(always)]
    pub fn has_flag(&self, flag: LiveRangeFlag) -> bool {
        self.num_fixed_uses_and_flags & ((flag as u32) << 24) != 0
    }
}

#[derive(Clone, Debug)]
struct Use {
    operand: Operand,
    pos: ProgPoint,
    slot: usize,
    next_use: UseIndex,
}

#[derive(Clone, Debug)]
struct Def {
    operand: Operand,
    pos: ProgPoint,
    slot: usize,
}

#[derive(Clone, Debug)]
struct LiveBundle {
    first_range: LiveRangeIndex,
    last_range: LiveRangeIndex,
    spillset: SpillSetIndex,
    allocation: Allocation,
    prio: u32, // recomputed after every bulk update
    spill_weight_and_props: u32,
}

impl LiveBundle {
    #[inline(always)]
    fn set_cached_spill_weight_and_props(&mut self, spill_weight: u32, minimal: bool, fixed: bool) {
        debug_assert!(spill_weight < ((1 << 30) - 1));
        self.spill_weight_and_props =
            spill_weight | (if minimal { 1 << 31 } else { 0 }) | (if fixed { 1 << 30 } else { 0 });
    }

    #[inline(always)]
    fn cached_minimal(&self) -> bool {
        self.spill_weight_and_props & (1 << 31) != 0
    }

    #[inline(always)]
    fn cached_fixed(&self) -> bool {
        self.spill_weight_and_props & (1 << 30) != 0
    }

    #[inline(always)]
    fn cached_spill_weight(&self) -> u32 {
        self.spill_weight_and_props & !((1 << 30) - 1)
    }
}

#[derive(Clone, Debug)]
struct SpillSet {
    bundles: LiveBundleVec,
    size: u32,
    class: RegClass,
    slot: SpillSlotIndex,
    reg_hint: Option<PReg>,
}

#[derive(Clone, Debug)]
struct VRegData {
    reg: VReg,
    def: DefIndex,
    blockparam: Block,
    first_range: LiveRangeIndex,
}

#[derive(Clone, Debug)]
struct PRegData {
    reg: PReg,
    allocations: LiveRangeSet,
}

/*
 * Environment setup:
 *
 * We have seven fundamental objects: LiveRange, LiveBundle, SpillSet, Use, Def, VReg, PReg.
 *
 * The relationship is as follows:
 *
 * LiveRange --(vreg)--> shared(VReg)
 * LiveRange --(bundle)--> shared(LiveBundle)
 * LiveRange --(def)--> owns(Def)
 * LiveRange --(use) --> list(Use)
 *
 * Use --(vreg)--> shared(VReg)
 *
 * Def --(vreg) --> owns(VReg)
 *
 * LiveBundle --(range)--> list(LiveRange)
 * LiveBundle --(spillset)--> shared(SpillSet)
 * LiveBundle --(parent)--> parent(LiveBundle)
 *
 * SpillSet --(parent)--> parent(SpillSet)
 * SpillSet --(bundles)--> list(LiveBundle)
 *
 * VReg --(range)--> list(LiveRange)
 *
 * PReg --(ranges)--> set(LiveRange)
 */

#[derive(Clone, Debug)]
struct Env<'a, F: Function> {
    func: &'a F,
    env: &'a MachineEnv,
    cfginfo: CFGInfo,
    liveins: Vec<BitVec>,
    /// Blockparam outputs: from-vreg, (end of) from-block, (start of)
    /// to-block, to-vreg. The field order is significant: these are sorted so
    /// that a scan over vregs, then blocks in each range, can scan in
    /// order through this (sorted) list and add allocs to the
    /// half-move list.
    blockparam_outs: Vec<(VRegIndex, Block, Block, VRegIndex)>,
    /// Blockparam inputs: to-vreg, (start of) to-block, (end of)
    /// from-block. As above for `blockparam_outs`, field order is
    /// significant.
    blockparam_ins: Vec<(VRegIndex, Block, Block)>,
    /// Blockparam allocs: block, idx, vreg, alloc. Info to describe
    /// blockparam locations at block entry, for metadata purposes
    /// (e.g. for the checker).
    blockparam_allocs: Vec<(Block, u32, VRegIndex, Allocation)>,

    ranges: Vec<LiveRange>,
    bundles: Vec<LiveBundle>,
    spillsets: Vec<SpillSet>,
    uses: Vec<Use>,
    defs: Vec<Def>,
    vregs: Vec<VRegData>,
    pregs: Vec<PRegData>,
    allocation_queue: PrioQueue,
    hot_code: LiveRangeSet,
    clobbers: Vec<Inst>, // Sorted list of insts with clobbers.

    spilled_bundles: Vec<LiveBundleIndex>,
    spillslots: Vec<SpillSlotData>,
    slots_by_size: Vec<SpillSlotList>,

    // When multiple fixed-register constraints are present on a
    // single VReg at a single program point (this can happen for,
    // e.g., call args that use the same value multiple times), we
    // remove all but one of the fixed-register constraints, make a
    // note here, and add a clobber with that PReg instread to keep
    // the register available. When we produce the final edit-list, we
    // will insert a copy from wherever the VReg's primary allocation
    // was to the approprate PReg.
    //
    // (progpoint, copy-from-preg, copy-to-preg)
    multi_fixed_reg_fixups: Vec<(ProgPoint, PRegIndex, PRegIndex)>,

    inserted_moves: Vec<InsertedMove>,

    // Output:
    edits: Vec<(u32, InsertMovePrio, Edit)>,
    allocs: Vec<Allocation>,
    inst_alloc_offsets: Vec<u32>,
    num_spillslots: u32,

    stats: Stats,

    // For debug output only: a list of textual annotations at every
    // ProgPoint to insert into the final allocated program listing.
    debug_annotations: std::collections::HashMap<ProgPoint, Vec<String>>,
}

#[derive(Clone, Debug)]
struct SpillSlotData {
    ranges: LiveRangeSet,
    class: RegClass,
    size: u32,
    alloc: Allocation,
    next_spillslot: SpillSlotIndex,
}

#[derive(Clone, Debug)]
struct SpillSlotList {
    first_spillslot: SpillSlotIndex,
    last_spillslot: SpillSlotIndex,
}

#[derive(Clone, Debug)]
struct PrioQueue {
    heap: std::collections::BinaryHeap<PrioQueueEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct PrioQueueEntry {
    prio: u32,
    bundle: LiveBundleIndex,
}

#[derive(Clone, Debug)]
struct LiveRangeSet {
    btree: BTreeMap<LiveRangeKey, LiveRangeIndex>,
}

#[derive(Clone, Copy, Debug)]
struct LiveRangeKey {
    from: u32,
    to: u32,
}

impl LiveRangeKey {
    fn from_range(range: &CodeRange) -> Self {
        Self {
            from: range.from.to_index(),
            to: range.to.to_index(),
        }
    }
}

impl std::cmp::PartialEq for LiveRangeKey {
    fn eq(&self, other: &Self) -> bool {
        self.to > other.from && self.from < other.to
    }
}
impl std::cmp::Eq for LiveRangeKey {}
impl std::cmp::PartialOrd for LiveRangeKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl std::cmp::Ord for LiveRangeKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        if self.to <= other.from {
            std::cmp::Ordering::Less
        } else if self.from >= other.to {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    }
}

struct PrioQueueComparator<'a> {
    prios: &'a [usize],
}
impl<'a> ContainerComparator for PrioQueueComparator<'a> {
    type Ix = LiveBundleIndex;
    fn compare(&self, a: Self::Ix, b: Self::Ix) -> std::cmp::Ordering {
        self.prios[a.index()].cmp(&self.prios[b.index()])
    }
}

impl PrioQueue {
    fn new() -> Self {
        PrioQueue {
            heap: std::collections::BinaryHeap::new(),
        }
    }

    fn insert(&mut self, bundle: LiveBundleIndex, prio: usize) {
        self.heap.push(PrioQueueEntry {
            prio: prio as u32,
            bundle,
        });
    }

    fn is_empty(self) -> bool {
        self.heap.is_empty()
    }

    fn pop(&mut self) -> Option<LiveBundleIndex> {
        self.heap.pop().map(|entry| entry.bundle)
    }
}

impl LiveRangeSet {
    pub(crate) fn new() -> Self {
        Self {
            btree: BTreeMap::new(),
        }
    }
}

fn spill_weight_from_policy(policy: OperandPolicy) -> u32 {
    match policy {
        OperandPolicy::Any => 1000,
        OperandPolicy::Reg | OperandPolicy::FixedReg(_) => 2000,
        _ => 0,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Requirement {
    Fixed(PReg),
    Register(RegClass),
    Any(RegClass),
}
impl Requirement {
    fn class(self) -> RegClass {
        match self {
            Requirement::Fixed(preg) => preg.class(),
            Requirement::Register(class) | Requirement::Any(class) => class,
        }
    }

    fn merge(self, other: Requirement) -> Option<Requirement> {
        if self.class() != other.class() {
            return None;
        }
        match (self, other) {
            (other, Requirement::Any(_)) | (Requirement::Any(_), other) => Some(other),
            (Requirement::Register(_), Requirement::Fixed(preg))
            | (Requirement::Fixed(preg), Requirement::Register(_)) => {
                Some(Requirement::Fixed(preg))
            }
            (Requirement::Register(_), Requirement::Register(_)) => Some(self),
            (Requirement::Fixed(a), Requirement::Fixed(b)) if a == b => Some(self),
            _ => None,
        }
    }
    fn from_operand(op: Operand) -> Requirement {
        match op.policy() {
            OperandPolicy::FixedReg(preg) => Requirement::Fixed(preg),
            OperandPolicy::Reg | OperandPolicy::Reuse(_) => Requirement::Register(op.class()),
            _ => Requirement::Any(op.class()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AllocRegResult {
    Allocated(Allocation),
    Conflict(LiveBundleVec),
    ConflictWithFixed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BundleProperties {
    minimal: bool,
    fixed: bool,
}

#[derive(Clone, Debug)]
struct InsertedMove {
    pos: ProgPoint,
    prio: InsertMovePrio,
    from_alloc: Allocation,
    to_alloc: Allocation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum InsertMovePrio {
    InEdgeMoves,
    BlockParam,
    Regular,
    MultiFixedReg,
    ReusedInput,
    OutEdgeMoves,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    initial_liverange_count: usize,
    merged_bundle_count: usize,
    process_bundle_count: usize,
    process_bundle_reg_probes_fixed: usize,
    process_bundle_reg_success_fixed: usize,
    process_bundle_reg_probes_any: usize,
    process_bundle_reg_success_any: usize,
    evict_bundle_event: usize,
    evict_bundle_count: usize,
    splits: usize,
    splits_clobbers: usize,
    splits_hot: usize,
    splits_conflicts: usize,
    splits_all: usize,
    final_liverange_count: usize,
    final_bundle_count: usize,
    spill_bundle_count: usize,
    spill_bundle_reg_probes: usize,
    spill_bundle_reg_success: usize,
    blockparam_ins_count: usize,
    blockparam_outs_count: usize,
    blockparam_allocs_count: usize,
    halfmoves_count: usize,
    edits_count: usize,
}

impl<'a, F: Function> Env<'a, F> {
    pub(crate) fn new(func: &'a F, env: &'a MachineEnv, cfginfo: CFGInfo) -> Self {
        Self {
            func,
            env,
            cfginfo,

            liveins: vec![],
            blockparam_outs: vec![],
            blockparam_ins: vec![],
            blockparam_allocs: vec![],
            bundles: vec![],
            ranges: vec![],
            spillsets: vec![],
            uses: vec![],
            defs: vec![],
            vregs: vec![],
            pregs: vec![],
            allocation_queue: PrioQueue::new(),
            clobbers: vec![],
            hot_code: LiveRangeSet::new(),
            spilled_bundles: vec![],
            spillslots: vec![],
            slots_by_size: vec![],

            multi_fixed_reg_fixups: vec![],
            inserted_moves: vec![],
            edits: vec![],
            allocs: vec![],
            inst_alloc_offsets: vec![],
            num_spillslots: 0,

            stats: Stats::default(),

            debug_annotations: std::collections::HashMap::new(),
        }
    }

    fn create_pregs_and_vregs(&mut self) {
        // Create RRegs from the RealRegUniverse.
        for &preg in &self.env.regs {
            self.pregs.push(PRegData {
                reg: preg,
                allocations: LiveRangeSet::new(),
            });
        }
        // Create VRegs from the vreg count.
        for idx in 0..self.func.num_vregs() {
            // We'll fill in the real details when we see the def.
            let reg = VReg::new(idx, RegClass::Int);
            self.add_vreg(VRegData {
                reg,
                def: DefIndex::invalid(),
                first_range: LiveRangeIndex::invalid(),
                blockparam: Block::invalid(),
            });
        }
        // Create allocations too.
        for inst in 0..self.func.insts() {
            let start = self.allocs.len() as u32;
            self.inst_alloc_offsets.push(start);
            for _ in 0..self.func.inst_operands(Inst::new(inst)).len() {
                self.allocs.push(Allocation::none());
            }
        }
    }

    fn add_vreg(&mut self, data: VRegData) -> VRegIndex {
        let idx = self.vregs.len();
        self.vregs.push(data);
        VRegIndex::new(idx)
    }

    fn create_liverange(&mut self, range: CodeRange) -> LiveRangeIndex {
        let idx = self.ranges.len();
        self.ranges.push(LiveRange {
            range,
            vreg: VRegIndex::invalid(),
            bundle: LiveBundleIndex::invalid(),
            uses_spill_weight: 0,
            num_fixed_uses_and_flags: 0,
            first_use: UseIndex::invalid(),
            last_use: UseIndex::invalid(),
            def: DefIndex::invalid(),
            next_in_bundle: LiveRangeIndex::invalid(),
            next_in_reg: LiveRangeIndex::invalid(),
        });
        LiveRangeIndex::new(idx)
    }

    /// Mark `range` as live for the given `vreg`. `num_ranges` is used to prevent
    /// excessive coalescing on pathological inputs.
    ///
    /// Returns the liverange that contains the given range.
    fn add_liverange_to_vreg(
        &mut self,
        vreg: VRegIndex,
        range: CodeRange,
        num_ranges: &mut usize,
    ) -> LiveRangeIndex {
        log::debug!("add_liverange_to_vreg: vreg {:?} range {:?}", vreg, range);
        const COALESCE_LIMIT: usize = 100_000;

        // Look for a single or contiguous sequence of existing live ranges that overlap with the
        // given range.

        let mut insert_after = LiveRangeIndex::invalid();
        let mut merged = LiveRangeIndex::invalid();
        let mut iter = self.vregs[vreg.index()].first_range;
        let mut prev = LiveRangeIndex::invalid();
        while iter.is_valid() {
            let existing = &mut self.ranges[iter.index()];
            log::debug!(" -> existing range: {:?}", existing);
            if range.from >= existing.range.to && *num_ranges < COALESCE_LIMIT {
                // New range comes fully after this one -- record it as a lower bound.
                insert_after = iter;
                prev = iter;
                iter = existing.next_in_reg;
                log::debug!("    -> lower bound");
                continue;
            }
            if range.to <= existing.range.from {
                // New range comes fully before this one -- we're found our spot.
                log::debug!("    -> upper bound (break search loop)");
                break;
            }
            // If we're here, then we overlap with at least one endpoint of the range.
            log::debug!("    -> must overlap");
            debug_assert!(range.overlaps(&existing.range));
            if merged.is_invalid() {
                // This is the first overlapping range. Extend to simply cover the new range.
                merged = iter;
                if range.from < existing.range.from {
                    existing.range.from = range.from;
                }
                if range.to > existing.range.to {
                    existing.range.to = range.to;
                }
                log::debug!(
                    "    -> extended range of existing range to {:?}",
                    existing.range
                );
                // Continue; there may be more ranges to merge with.
                prev = iter;
                iter = existing.next_in_reg;
                continue;
            }
            // We overlap but we've already extended the first overlapping existing liverange, so
            // we need to do a true merge instead.
            log::debug!("    -> merging {:?} into {:?}", iter, merged);
            log::debug!(
                "    -> before: merged {:?}: {:?}",
                merged,
                self.ranges[merged.index()]
            );
            debug_assert!(
                self.ranges[iter.index()].range.from >= self.ranges[merged.index()].range.from
            ); // Because we see LRs in order.
            if self.ranges[iter.index()].range.to > self.ranges[merged.index()].range.to {
                self.ranges[merged.index()].range.to = self.ranges[iter.index()].range.to;
            }
            if self.ranges[iter.index()].def.is_valid() {
                self.ranges[merged.index()].def = self.ranges[iter.index()].def;
            }
            self.distribute_liverange_uses(vreg, iter, merged);
            log::debug!(
                "    -> after: merged {:?}: {:?}",
                merged,
                self.ranges[merged.index()]
            );

            // Remove from list of liveranges for this vreg.
            let next = self.ranges[iter.index()].next_in_reg;
            if prev.is_valid() {
                self.ranges[prev.index()].next_in_reg = next;
            } else {
                self.vregs[vreg.index()].first_range = next;
            }
            // `prev` remains the same (we deleted current range).
            iter = next;
        }

        // If we get here and did not merge into an existing liverange or liveranges, then we need
        // to create a new one.
        if merged.is_invalid() {
            let lr = self.create_liverange(range);
            self.ranges[lr.index()].vreg = vreg;
            if insert_after.is_valid() {
                let next = self.ranges[insert_after.index()].next_in_reg;
                self.ranges[lr.index()].next_in_reg = next;
                self.ranges[insert_after.index()].next_in_reg = lr;
            } else {
                self.ranges[lr.index()].next_in_reg = self.vregs[vreg.index()].first_range;
                self.vregs[vreg.index()].first_range = lr;
            }
            *num_ranges += 1;
            lr
        } else {
            merged
        }
    }

    fn distribute_liverange_uses(
        &mut self,
        vreg: VRegIndex,
        from: LiveRangeIndex,
        into: LiveRangeIndex,
    ) {
        log::debug!("distribute from {:?} to {:?}", from, into);
        assert_eq!(
            self.ranges[from.index()].vreg,
            self.ranges[into.index()].vreg
        );
        let from_range = self.ranges[from.index()].range;
        let into_range = self.ranges[into.index()].range;
        // For every use in `from`...
        let mut prev = UseIndex::invalid();
        let mut iter = self.ranges[from.index()].first_use;
        while iter.is_valid() {
            let usedata = &mut self.uses[iter.index()];
            // If we have already passed `into`, we're done.
            if usedata.pos >= into_range.to {
                break;
            }
            // If this use is within the range of `into`, move it over.
            if into_range.contains_point(usedata.pos) {
                log::debug!(" -> moving {:?}", iter);
                let next = usedata.next_use;
                if prev.is_valid() {
                    self.uses[prev.index()].next_use = next;
                } else {
                    self.ranges[from.index()].first_use = next;
                }
                if iter == self.ranges[from.index()].last_use {
                    self.ranges[from.index()].last_use = prev;
                }
                // `prev` remains the same.
                self.update_liverange_stats_on_remove_use(from, iter);
                // This may look inefficient but because we are always merging
                // non-overlapping LiveRanges, all uses will be at the beginning
                // or end of the existing use-list; both cases are optimized.
                self.insert_use_into_liverange_and_update_stats(into, iter);
                iter = next;
            } else {
                prev = iter;
                iter = usedata.next_use;
            }
        }

        // Distribute def too if `from` has a def and the def is in range of `into_range`.
        if self.ranges[from.index()].def.is_valid() {
            let def_idx = self.vregs[vreg.index()].def;
            if from_range.contains_point(self.defs[def_idx.index()].pos) {
                self.ranges[into.index()].def = def_idx;
            }
        }
    }

    fn update_liverange_stats_on_remove_use(&mut self, from: LiveRangeIndex, u: UseIndex) {
        log::debug!("remove use {:?} from lr {:?}", u, from);
        debug_assert!(u.is_valid());
        let usedata = &self.uses[u.index()];
        let lrdata = &mut self.ranges[from.index()];
        if let OperandPolicy::FixedReg(_) = usedata.operand.policy() {
            lrdata.dec_num_fixed_uses();
        }
        log::debug!(
            "  -> subtract {} from uses_spill_weight {}; now {}",
            spill_weight_from_policy(usedata.operand.policy()),
            lrdata.uses_spill_weight,
            lrdata.uses_spill_weight - spill_weight_from_policy(usedata.operand.policy()),
        );

        lrdata.uses_spill_weight -= spill_weight_from_policy(usedata.operand.policy());
    }

    fn insert_use_into_liverange_and_update_stats(&mut self, into: LiveRangeIndex, u: UseIndex) {
        let insert_pos = self.uses[u.index()].pos;
        let first = self.ranges[into.index()].first_use;
        self.uses[u.index()].next_use = UseIndex::invalid();
        if first.is_invalid() {
            // Empty list.
            self.ranges[into.index()].first_use = u;
            self.ranges[into.index()].last_use = u;
        } else if insert_pos > self.uses[self.ranges[into.index()].last_use.index()].pos {
            // After tail.
            let tail = self.ranges[into.index()].last_use;
            self.uses[tail.index()].next_use = u;
            self.ranges[into.index()].last_use = u;
        } else {
            // Otherwise, scan linearly to find insertion position.
            let mut prev = UseIndex::invalid();
            let mut iter = first;
            while iter.is_valid() {
                if self.uses[iter.index()].pos > insert_pos {
                    break;
                }
                prev = iter;
                iter = self.uses[iter.index()].next_use;
            }
            self.uses[u.index()].next_use = iter;
            if prev.is_valid() {
                self.uses[prev.index()].next_use = u;
            } else {
                self.ranges[into.index()].first_use = u;
            }
            if iter.is_invalid() {
                self.ranges[into.index()].last_use = u;
            }
        }

        // Update stats.
        let policy = self.uses[u.index()].operand.policy();
        if let OperandPolicy::FixedReg(_) = policy {
            self.ranges[into.index()].inc_num_fixed_uses();
        }
        log::debug!(
            "insert use {:?} into lr {:?} with weight {}",
            u,
            into,
            spill_weight_from_policy(policy)
        );
        self.ranges[into.index()].uses_spill_weight += spill_weight_from_policy(policy);
        log::debug!("  -> now {}", self.ranges[into.index()].uses_spill_weight);
    }

    fn find_vreg_liverange_for_pos(
        &self,
        vreg: VRegIndex,
        pos: ProgPoint,
    ) -> Option<LiveRangeIndex> {
        let mut range = self.vregs[vreg.index()].first_range;
        while range.is_valid() {
            if self.ranges[range.index()].range.contains_point(pos) {
                return Some(range);
            }
            range = self.ranges[range.index()].next_in_reg;
        }
        None
    }

    fn add_liverange_to_preg(&mut self, range: CodeRange, reg: PReg) {
        let preg_idx = PRegIndex::new(reg.index());
        let lr = self.create_liverange(range);
        self.pregs[preg_idx.index()]
            .allocations
            .btree
            .insert(LiveRangeKey::from_range(&range), lr);
    }

    fn compute_liveness(&mut self) {
        // Create initial LiveIn bitsets.
        for _ in 0..self.func.blocks() {
            self.liveins.push(BitVec::new());
        }

        let num_vregs = self.func.num_vregs();

        let mut num_ranges = 0;

        // Create Uses and Defs referring to VRegs, and place the Uses
        // in LiveRanges.
        //
        // We iterate backward, so as long as blocks are well-ordered
        // (in RPO), we see uses before defs.
        //
        // Because of this, we can construct live ranges in one pass,
        // i.e., considering each block once, propagating live
        // registers backward across edges to a bitset at each block
        // exit point, gen'ing at uses, kill'ing at defs, and meeting
        // with a union.
        let mut block_to_postorder: SmallVec<[Option<u32>; 16]> =
            smallvec![None; self.func.blocks()];
        for i in 0..self.cfginfo.postorder.len() {
            let block = self.cfginfo.postorder[i];
            block_to_postorder[block.index()] = Some(i as u32);
        }

        // Track current LiveRange for each vreg.
        let mut vreg_ranges: Vec<LiveRangeIndex> =
            vec![LiveRangeIndex::invalid(); self.func.num_vregs()];

        for i in 0..self.cfginfo.postorder.len() {
            // (avoid borrowing `self`)
            let block = self.cfginfo.postorder[i];
            block_to_postorder[block.index()] = Some(i as u32);

            // Init live-set to union of liveins from successors
            // (excluding backedges; those are handled below).
            let mut live = BitVec::with_capacity(num_vregs);
            for &succ in self.func.block_succs(block) {
                live.or(&self.liveins[succ.index()]);
            }

            // Initially, registers are assumed live for the whole block.
            for vreg in live.iter() {
                let range = CodeRange {
                    from: self.cfginfo.block_entry[block.index()],
                    to: self.cfginfo.block_exit[block.index()].next(),
                };
                log::debug!(
                    "vreg {:?} live at end of block --> create range {:?}",
                    VRegIndex::new(vreg),
                    range
                );
                let lr = self.add_liverange_to_vreg(VRegIndex::new(vreg), range, &mut num_ranges);
                vreg_ranges[vreg] = lr;
            }

            // Create vreg data for blockparams.
            for param in self.func.block_params(block) {
                self.vregs[param.vreg()].reg = *param;
                self.vregs[param.vreg()].blockparam = block;
            }

            let insns = self.func.block_insns(block);

            // If the last instruction is a branch (rather than
            // return), create blockparam_out entries.
            if self.func.is_branch(insns.last()) {
                let operands = self.func.inst_operands(insns.last());
                let mut i = 0;
                for &succ in self.func.block_succs(block) {
                    for &blockparam in self.func.block_params(succ) {
                        let from_vreg = VRegIndex::new(operands[i].vreg().vreg());
                        let blockparam_vreg = VRegIndex::new(blockparam.vreg());
                        self.blockparam_outs
                            .push((from_vreg, block, succ, blockparam_vreg));
                        i += 1;
                    }
                }
            }

            // For each instruction, in reverse order, process
            // operands and clobbers.
            for inst in insns.rev().iter() {
                if self.func.inst_clobbers(inst).len() > 0 {
                    self.clobbers.push(inst);
                }
                // Mark clobbers with CodeRanges on PRegs.
                for i in 0..self.func.inst_clobbers(inst).len() {
                    // don't borrow `self`
                    let clobber = self.func.inst_clobbers(inst)[i];
                    let range = CodeRange {
                        from: ProgPoint::before(inst),
                        to: ProgPoint::before(inst.next()),
                    };
                    self.add_liverange_to_preg(range, clobber);
                }

                // Does the instruction have any input-reusing
                // outputs? This is important below to establish
                // proper interference wrt other inputs.
                let mut reused_input = None;
                for op in self.func.inst_operands(inst) {
                    if let OperandPolicy::Reuse(i) = op.policy() {
                        reused_input = Some(i);
                        break;
                    }
                }

                // Process defs and uses.
                for i in 0..self.func.inst_operands(inst).len() {
                    // don't borrow `self`
                    let operand = self.func.inst_operands(inst)[i];
                    match operand.kind() {
                        OperandKind::Def => {
                            // Create the Def object.
                            let pos = match operand.pos() {
                                OperandPos::Before | OperandPos::Both => ProgPoint::before(inst),
                                OperandPos::After => ProgPoint::after(inst),
                            };
                            let def = DefIndex(self.defs.len() as u32);
                            self.defs.push(Def {
                                operand,
                                pos,
                                slot: i,
                            });

                            log::debug!("Def of {} at {:?}", operand.vreg(), pos);

                            // Fill in vreg's actual data.
                            debug_assert!(self.vregs[operand.vreg().vreg()].def.is_invalid());
                            self.vregs[operand.vreg().vreg()].reg = operand.vreg();
                            self.vregs[operand.vreg().vreg()].def = def;

                            // Trim the range for this vreg to start
                            // at `pos` if it previously ended at the
                            // start of this block (i.e. was not
                            // merged into some larger LiveRange due
                            // to out-of-order blocks).
                            let mut lr = vreg_ranges[operand.vreg().vreg()];
                            log::debug!(" -> has existing LR {:?}", lr);
                            // If there was no liverange (dead def), create a trivial one.
                            if lr.is_invalid() {
                                lr = self.add_liverange_to_vreg(
                                    VRegIndex::new(operand.vreg().vreg()),
                                    CodeRange {
                                        from: pos,
                                        to: pos.next(),
                                    },
                                    &mut num_ranges,
                                );
                                log::debug!(" -> invalid; created {:?}", lr);
                            }
                            if self.ranges[lr.index()].range.from
                                == self.cfginfo.block_entry[block.index()]
                            {
                                log::debug!(" -> started at block start; trimming to {:?}", pos);
                                self.ranges[lr.index()].range.from = pos;
                            }
                            // Note that the liverange contains a def.
                            self.ranges[lr.index()].def = def;
                            // Remove from live-set.
                            live.set(operand.vreg().vreg(), false);
                            vreg_ranges[operand.vreg().vreg()] = LiveRangeIndex::invalid();
                        }
                        OperandKind::Use => {
                            // Establish where the use occurs.
                            let mut pos = match operand.pos() {
                                OperandPos::Before => ProgPoint::before(inst),
                                OperandPos::Both | OperandPos::After => ProgPoint::after(inst),
                            };
                            // If there are any reused inputs in this
                            // instruction, and this is *not* the
                            // reused input, force `pos` to
                            // `After`. (See note below for why; it's
                            // very subtle!)
                            if reused_input.is_some() && reused_input.unwrap() != i {
                                pos = ProgPoint::after(inst);
                            }
                            // If this is a branch, extend `pos` to
                            // the end of the block. (Branch uses are
                            // blockparams and need to be live at the
                            // end of the block.
                            if self.func.is_branch(inst) {
                                pos = self.cfginfo.block_exit[block.index()];
                            }

                            // Create the actual use object.
                            let u = UseIndex(self.uses.len() as u32);
                            self.uses.push(Use {
                                operand,
                                pos,
                                slot: i,
                                next_use: UseIndex::invalid(),
                            });

                            // Create/extend the LiveRange and add the use to the range.
                            let range = CodeRange {
                                from: self.cfginfo.block_entry[block.index()],
                                to: pos.next(),
                            };
                            let lr = self.add_liverange_to_vreg(
                                VRegIndex::new(operand.vreg().vreg()),
                                range,
                                &mut num_ranges,
                            );
                            vreg_ranges[operand.vreg().vreg()] = lr;

                            log::debug!("Use of {:?} at {:?} -> {:?} -> {:?}", operand, pos, u, lr);

                            self.insert_use_into_liverange_and_update_stats(lr, u);

                            // Add to live-set.
                            live.set(operand.vreg().vreg(), true);
                        }
                    }
                }
            }

            // Block parameters define vregs at the very beginning of
            // the block. Remove their live vregs from the live set
            // here.
            for vreg in self.func.block_params(block) {
                if live.get(vreg.vreg()) {
                    live.set(vreg.vreg(), false);
                } else {
                    // Create trivial liverange if blockparam is dead.
                    let start = self.cfginfo.block_entry[block.index()];
                    self.add_liverange_to_vreg(
                        VRegIndex::new(vreg.vreg()),
                        CodeRange {
                            from: start,
                            to: start.next(),
                        },
                        &mut num_ranges,
                    );
                }
                // add `blockparam_ins` entries.
                let vreg_idx = VRegIndex::new(vreg.vreg());
                for &pred in self.func.block_preds(block) {
                    self.blockparam_ins.push((vreg_idx, block, pred));
                }
            }

            // Loop-handling: to handle backedges, rather than running
            // a fixpoint loop, we add a live-range for every value
            // live at the beginning of the loop over the whole loop
            // body.
            //
            // To determine what the "loop body" consists of, we find
            // the transitively minimum-reachable traversal index in
            // our traversal order before the current block
            // index. When we discover a backedge, *all* block indices
            // within the traversal range are considered part of the
            // loop body.  This is guaranteed correct (though perhaps
            // an overapproximation) even for irreducible control
            // flow, because it will find all blocks to which the
            // liveness could flow backward over which we've already
            // scanned, and it should give good results for reducible
            // control flow with properly ordered blocks.
            let mut min_pred = i;
            let mut loop_scan = i;
            log::debug!(
                "looking for loops from postorder#{} (block{})",
                i,
                self.cfginfo.postorder[i].index()
            );
            while loop_scan >= min_pred {
                let block = self.cfginfo.postorder[loop_scan];
                log::debug!(
                    " -> scan at postorder#{} (block{})",
                    loop_scan,
                    block.index()
                );
                for &pred in self.func.block_preds(block) {
                    log::debug!(
                        " -> pred block{} (postorder#{})",
                        pred.index(),
                        block_to_postorder[pred.index()].unwrap_or(min_pred as u32)
                    );
                    min_pred = std::cmp::min(
                        min_pred,
                        block_to_postorder[pred.index()].unwrap_or(min_pred as u32) as usize,
                    );
                    log::debug!(" -> min_pred = {}", min_pred);
                }
                if loop_scan == 0 {
                    break;
                }
                loop_scan -= 1;
            }

            if min_pred < i {
                // We have one or more backedges, and the loop body is
                // (conservatively) postorder[min_pred..i]. Find a
                // range that covers all of those blocks.
                let loop_blocks = &self.cfginfo.postorder[min_pred..=i];
                let loop_begin = loop_blocks
                    .iter()
                    .map(|b| self.cfginfo.block_entry[b.index()])
                    .min()
                    .unwrap();
                let loop_end = loop_blocks
                    .iter()
                    .map(|b| self.cfginfo.block_exit[b.index()])
                    .max()
                    .unwrap();
                let loop_range = CodeRange {
                    from: loop_begin,
                    to: loop_end,
                };
                log::debug!(
                    "found backedge wrt postorder: postorder#{}..postorder#{}",
                    min_pred,
                    i
                );
                log::debug!(" -> loop range {:?}", loop_range);
                for &loopblock in loop_blocks {
                    self.liveins[loopblock.index()].or(&live);
                }
                for vreg in live.iter() {
                    log::debug!(
                        "vreg {:?} live at top of loop (block {:?}) -> range {:?}",
                        VRegIndex::new(vreg),
                        block,
                        loop_range,
                    );
                    self.add_liverange_to_vreg(VRegIndex::new(vreg), loop_range, &mut num_ranges);
                }
            }

            log::debug!("liveins at block {:?} = {:?}", block, live);
            self.liveins[block.index()] = live;
        }

        // Do a cleanup pass: if there are any LiveRanges with
        // multiple uses (or defs) at the same ProgPoint and there is
        // more than one FixedReg constraint at that ProgPoint, we
        // need to record all but one of them in a special fixup list
        // and handle them later; otherwise, bundle-splitting to
        // create minimal bundles becomes much more complex (we would
        // have to split the multiple uses at the same progpoint into
        // different bundles, which breaks invariants related to
        // disjoint ranges and bundles).
        for vreg in 0..self.vregs.len() {
            let mut iter = self.vregs[vreg].first_range;
            while iter.is_valid() {
                log::debug!(
                    "multi-fixed-reg cleanup: vreg {:?} range {:?}",
                    VRegIndex::new(vreg),
                    iter
                );
                let mut last_point = None;
                let mut seen_fixed_for_vreg: SmallVec<[VReg; 16]> = smallvec![];
                let mut first_preg: SmallVec<[PRegIndex; 16]> = smallvec![];
                let mut extra_clobbers: SmallVec<[(PReg, Inst); 8]> = smallvec![];
                let mut fixup_multi_fixed_vregs = |pos: ProgPoint,
                                                   op: &mut Operand,
                                                   fixups: &mut Vec<(
                    ProgPoint,
                    PRegIndex,
                    PRegIndex,
                )>| {
                    if last_point.is_some() && Some(pos) != last_point {
                        seen_fixed_for_vreg.clear();
                        first_preg.clear();
                    }
                    last_point = Some(pos);

                    if let OperandPolicy::FixedReg(preg) = op.policy() {
                        let vreg_idx = VRegIndex::new(op.vreg().vreg());
                        let preg_idx = PRegIndex::new(preg.index());
                        log::debug!(
                            "at pos {:?}, vreg {:?} has fixed constraint to preg {:?}",
                            pos,
                            vreg_idx,
                            preg_idx
                        );
                        if let Some(idx) = seen_fixed_for_vreg.iter().position(|r| *r == op.vreg())
                        {
                            let orig_preg = first_preg[idx];
                            log::debug!(" -> duplicate; switching to policy Reg");
                            fixups.push((pos, orig_preg, preg_idx));
                            *op = Operand::new(op.vreg(), OperandPolicy::Reg, op.kind(), op.pos());
                            extra_clobbers.push((preg, pos.inst));
                        } else {
                            seen_fixed_for_vreg.push(op.vreg());
                            first_preg.push(preg_idx);
                        }
                    }
                };

                if self.ranges[iter.index()].def.is_valid() {
                    let def_idx = self.vregs[vreg].def;
                    let pos = self.defs[def_idx.index()].pos;
                    fixup_multi_fixed_vregs(
                        pos,
                        &mut self.defs[def_idx.index()].operand,
                        &mut self.multi_fixed_reg_fixups,
                    );
                }

                let mut use_iter = self.ranges[iter.index()].first_use;
                while use_iter.is_valid() {
                    let pos = self.uses[use_iter.index()].pos;
                    fixup_multi_fixed_vregs(
                        pos,
                        &mut self.uses[use_iter.index()].operand,
                        &mut self.multi_fixed_reg_fixups,
                    );
                    use_iter = self.uses[use_iter.index()].next_use;
                }

                for (clobber, inst) in extra_clobbers {
                    let range = CodeRange {
                        from: ProgPoint::before(inst),
                        to: ProgPoint::before(inst.next()),
                    };
                    self.add_liverange_to_preg(range, clobber);
                }

                iter = self.ranges[iter.index()].next_in_reg;
            }
        }

        self.clobbers.sort();
        self.blockparam_ins.sort();
        self.blockparam_outs.sort();

        self.stats.initial_liverange_count = self.ranges.len();
        self.stats.blockparam_ins_count = self.blockparam_ins.len();
        self.stats.blockparam_outs_count = self.blockparam_outs.len();
    }

    fn compute_hot_code(&mut self) {
        // Initialize hot_code to contain inner loops only.
        let mut header = Block::invalid();
        let mut backedge = Block::invalid();
        for block in 0..self.func.blocks() {
            let block = Block::new(block);
            let max_backedge = self
                .func
                .block_preds(block)
                .iter()
                .filter(|b| b.index() >= block.index())
                .max();
            if let Some(&b) = max_backedge {
                header = block;
                backedge = b;
            }
            if block == backedge {
                // We've traversed a loop body without finding a deeper loop. Mark the whole body
                // as hot.
                let from = self.cfginfo.block_entry[header.index()];
                let to = self.cfginfo.block_exit[backedge.index()].next();
                let range = CodeRange { from, to };
                let lr = self.create_liverange(range);
                self.hot_code
                    .btree
                    .insert(LiveRangeKey::from_range(&range), lr);
            }
        }
    }

    fn create_bundle(&mut self) -> LiveBundleIndex {
        let bundle = self.bundles.len();
        self.bundles.push(LiveBundle {
            allocation: Allocation::none(),
            first_range: LiveRangeIndex::invalid(),
            last_range: LiveRangeIndex::invalid(),
            spillset: SpillSetIndex::invalid(),
            prio: 0,
            spill_weight_and_props: 0,
        });
        LiveBundleIndex::new(bundle)
    }

    fn try_merge_reused_register(&mut self, from: VRegIndex, to: VRegIndex) {
        log::debug!("try_merge_reused_register: from {:?} to {:?}", from, to);
        let def_idx = self.vregs[to.index()].def;
        log::debug!(" -> def_idx = {:?}", def_idx);
        debug_assert!(def_idx.is_valid());
        let def = &mut self.defs[def_idx.index()];
        let def_point = def.pos;
        log::debug!(" -> def_point = {:?}", def_point);

        // Can't merge if def happens at use-point.
        if def_point.pos == InstPosition::Before {
            return;
        }

        // Find the corresponding liverange for the use at the def-point.
        let use_lr_at_def = self.find_vreg_liverange_for_pos(from, def_point);
        log::debug!(" -> use_lr_at_def = {:?}", use_lr_at_def);

        // If the use is not live at the def (i.e. this inst is its last use), we can merge.
        if use_lr_at_def.is_none() {
            // Find the bundles and merge. Note that bundles have not been split
            // yet so every liverange in the vreg will have the same bundle (so
            // no need to look up the proper liverange here).
            let from_bundle = self.ranges[self.vregs[from.index()].first_range.index()].bundle;
            let to_bundle = self.ranges[self.vregs[to.index()].first_range.index()].bundle;
            log::debug!(" -> merging from {:?} to {:?}", from_bundle, to_bundle);
            self.merge_bundles(from_bundle, to_bundle);
            return;
        }

        log::debug!(" -> no merge");

        // Note: there may be other cases where it would benefit us to split the
        // LiveRange and bundle for the input at the def-point, allowing us to
        // avoid a copy. However, the cases where this helps in IonMonkey (only
        // memory uses after the definition, seemingly) appear to be marginal at
        // best.
    }

    fn merge_bundles(&mut self, from: LiveBundleIndex, to: LiveBundleIndex) -> bool {
        if from == to {
            // Merge bundle into self -- trivial merge.
            return true;
        }
        log::debug!(
            "merging from bundle{} to bundle{}",
            from.index(),
            to.index()
        );

        let vreg_from = self.ranges[self.bundles[from.index()].first_range.index()].vreg;
        let vreg_to = self.ranges[self.bundles[to.index()].first_range.index()].vreg;
        // Both bundles must deal with the same RegClass. All vregs in a bundle
        // have to have the same regclass (because bundles start with one vreg
        // and all merging happens here) so we can just sample the first vreg of
        // each bundle.
        if self.vregs[vreg_from.index()].reg.class() != self.vregs[vreg_to.index()].reg.class() {
            return false;
        }

        // Check for overlap in LiveRanges.
        let mut iter0 = self.bundles[from.index()].first_range;
        let mut iter1 = self.bundles[to.index()].first_range;
        let mut range_count = 0;
        while iter0.is_valid() && iter1.is_valid() {
            range_count += 1;
            if range_count > 200 {
                // Limit merge complexity.
                return false;
            }

            if self.ranges[iter0.index()].range.from >= self.ranges[iter1.index()].range.to {
                iter1 = self.ranges[iter1.index()].next_in_bundle;
            } else if self.ranges[iter1.index()].range.from >= self.ranges[iter0.index()].range.to {
                iter0 = self.ranges[iter0.index()].next_in_bundle;
            } else {
                // Overlap -- cannot merge.
                return false;
            }
        }

        // If we reach here, then the bundles do not overlap -- merge them!
        // We do this with a merge-sort-like scan over both chains, removing
        // from `to` (`iter1`) and inserting into `from` (`iter0`).
        let mut iter0 = self.bundles[from.index()].first_range;
        let mut iter1 = self.bundles[to.index()].first_range;
        if iter0.is_invalid() {
            // `from` bundle is empty -- trivial merge.
            return true;
        }
        if iter1.is_invalid() {
            // `to` bundle is empty -- just move head/tail pointers over from
            // `from` and set `bundle` up-link on all ranges.
            let head = self.bundles[from.index()].first_range;
            let tail = self.bundles[from.index()].last_range;
            self.bundles[to.index()].first_range = head;
            self.bundles[to.index()].last_range = tail;
            self.bundles[from.index()].first_range = LiveRangeIndex::invalid();
            self.bundles[from.index()].last_range = LiveRangeIndex::invalid();
            while iter0.is_valid() {
                self.ranges[iter0.index()].bundle = from;
                iter0 = self.ranges[iter0.index()].next_in_bundle;
            }
            return true;
        }

        // Two non-empty chains of LiveRanges: traverse both simultaneously and
        // merge links into `from`.
        let mut prev = LiveRangeIndex::invalid();
        while iter0.is_valid() || iter1.is_valid() {
            // Pick the next range.
            let next_range_iter = if iter0.is_valid() {
                if iter1.is_valid() {
                    if self.ranges[iter0.index()].range.from
                        <= self.ranges[iter1.index()].range.from
                    {
                        &mut iter0
                    } else {
                        &mut iter1
                    }
                } else {
                    &mut iter0
                }
            } else {
                &mut iter1
            };
            let next = *next_range_iter;
            *next_range_iter = self.ranges[next.index()].next_in_bundle;

            // link from prev.
            if prev.is_valid() {
                self.ranges[prev.index()].next_in_bundle = next;
            } else {
                self.bundles[to.index()].first_range = next;
            }
            self.bundles[to.index()].last_range = next;
            self.ranges[next.index()].bundle = to;
            prev = next;
        }
        self.bundles[from.index()].first_range = LiveRangeIndex::invalid();
        self.bundles[from.index()].last_range = LiveRangeIndex::invalid();

        true
    }

    fn insert_liverange_into_bundle(&mut self, bundle: LiveBundleIndex, lr: LiveRangeIndex) {
        self.ranges[lr.index()].next_in_bundle = LiveRangeIndex::invalid();
        self.ranges[lr.index()].bundle = bundle;
        if self.bundles[bundle.index()].first_range.is_invalid() {
            // Empty bundle.
            self.bundles[bundle.index()].first_range = lr;
            self.bundles[bundle.index()].last_range = lr;
        } else if self.ranges[self.bundles[bundle.index()].first_range.index()]
            .range
            .to
            <= self.ranges[lr.index()].range.from
        {
            // After last range in bundle.
            let last = self.bundles[bundle.index()].last_range;
            self.ranges[last.index()].next_in_bundle = lr;
            self.bundles[bundle.index()].last_range = lr;
        } else {
            // Find location to insert.
            let mut iter = self.bundles[bundle.index()].first_range;
            let mut insert_after = LiveRangeIndex::invalid();
            let insert_range = self.ranges[lr.index()].range;
            while iter.is_valid() {
                debug_assert!(!self.ranges[iter.index()].range.overlaps(&insert_range));
                if self.ranges[iter.index()].range.to <= insert_range.from {
                    break;
                }
                insert_after = iter;
                iter = self.ranges[iter.index()].next_in_bundle;
            }
            if insert_after.is_valid() {
                self.ranges[insert_after.index()].next_in_bundle = lr;
                if self.bundles[bundle.index()].last_range == insert_after {
                    self.bundles[bundle.index()].last_range = lr;
                }
            } else {
                let next = self.bundles[bundle.index()].first_range;
                self.ranges[lr.index()].next_in_bundle = next;
                self.bundles[bundle.index()].first_range = lr;
            }
        }
    }

    fn merge_vreg_bundles(&mut self) {
        // Create a bundle for every vreg, initially.
        log::debug!("merge_vreg_bundles: creating vreg bundles");
        for vreg in 0..self.vregs.len() {
            let vreg = VRegIndex::new(vreg);
            if self.vregs[vreg.index()].first_range.is_invalid() {
                continue;
            }
            let bundle = self.create_bundle();
            let mut range = self.vregs[vreg.index()].first_range;
            while range.is_valid() {
                self.insert_liverange_into_bundle(bundle, range);
                range = self.ranges[range.index()].next_in_reg;
            }
            log::debug!("vreg v{} gets bundle{}", vreg.index(), bundle.index());
        }

        for inst in 0..self.func.insts() {
            let inst = Inst::new(inst);

            // Attempt to merge Reuse-policy operand outputs with the corresponding
            // inputs.
            for operand_idx in 0..self.func.inst_operands(inst).len() {
                let operand = self.func.inst_operands(inst)[operand_idx];
                if let OperandPolicy::Reuse(input_idx) = operand.policy() {
                    log::debug!(
                        "trying to merge use and def at reused-op {} on inst{}",
                        operand_idx,
                        inst.index()
                    );
                    assert_eq!(operand.kind(), OperandKind::Def);
                    assert_eq!(operand.pos(), OperandPos::After);
                    let input_vreg =
                        VRegIndex::new(self.func.inst_operands(inst)[input_idx].vreg().vreg());
                    let output_vreg = VRegIndex::new(operand.vreg().vreg());
                    self.try_merge_reused_register(input_vreg, output_vreg);
                }
            }

            // Attempt to merge move srcs and dests.
            if let Some((src_vreg, dst_vreg)) = self.func.is_move(inst) {
                log::debug!("trying to merge move src {} to dst {}", src_vreg, dst_vreg);
                let src_bundle =
                    self.ranges[self.vregs[src_vreg.vreg()].first_range.index()].bundle;
                assert!(src_bundle.is_valid());
                let dest_bundle =
                    self.ranges[self.vregs[dst_vreg.vreg()].first_range.index()].bundle;
                assert!(dest_bundle.is_valid());
                self.merge_bundles(/* from */ dest_bundle, /* to */ src_bundle);
            }
        }

        // Attempt to merge blockparams with their inputs.
        for i in 0..self.blockparam_outs.len() {
            let (from_vreg, _, _, to_vreg) = self.blockparam_outs[i];
            log::debug!(
                "trying to merge blockparam v{} with input v{}",
                to_vreg.index(),
                from_vreg.index()
            );
            let to_bundle = self.ranges[self.vregs[to_vreg.index()].first_range.index()].bundle;
            assert!(to_bundle.is_valid());
            let from_bundle = self.ranges[self.vregs[from_vreg.index()].first_range.index()].bundle;
            assert!(from_bundle.is_valid());
            log::debug!(
                " -> from bundle{} to bundle{}",
                from_bundle.index(),
                to_bundle.index()
            );
            self.merge_bundles(from_bundle, to_bundle);
        }

        log::debug!("done merging bundles");
    }

    fn compute_bundle_prio(&self, bundle: LiveBundleIndex) -> u32 {
        // The priority is simply the total "length" -- the number of
        // instructions covered by all LiveRanges.
        let mut iter = self.bundles[bundle.index()].first_range;
        let mut total = 0;
        while iter.is_valid() {
            total += self.ranges[iter.index()].range.len() as u32;
            iter = self.ranges[iter.index()].next_in_bundle;
        }
        total
    }

    fn queue_bundles(&mut self) {
        for vreg in 0..self.vregs.len() {
            let vreg = VRegIndex::new(vreg);
            let mut lr = self.vregs[vreg.index()].first_range;
            while lr.is_valid() {
                let bundle = self.ranges[lr.index()].bundle;
                if self.bundles[bundle.index()].first_range == lr {
                    // First time seeing `bundle`: allocate a spillslot for it,
                    // compute its priority, and enqueue it.
                    let ssidx = SpillSetIndex::new(self.spillsets.len());
                    let reg = self.vregs[vreg.index()].reg;
                    let size = self.func.spillslot_size(reg.class(), reg) as u32;
                    self.spillsets.push(SpillSet {
                        bundles: smallvec![],
                        slot: SpillSlotIndex::invalid(),
                        size,
                        class: reg.class(),
                        reg_hint: None,
                    });
                    self.bundles[bundle.index()].spillset = ssidx;
                    let prio = self.compute_bundle_prio(bundle);
                    self.bundles[bundle.index()].prio = prio;
                    self.recompute_bundle_properties(bundle);
                    self.allocation_queue.insert(bundle, prio as usize);
                }

                // Keep going even if we handled one bundle for this vreg above:
                // if we split a vreg's liveranges into multiple bundles, we
                // need to hit all the bundles.
                lr = self.ranges[lr.index()].next_in_bundle;
            }
        }

        self.stats.merged_bundle_count = self.allocation_queue.heap.len();
    }

    fn process_bundles(&mut self) {
        let mut count = 0;
        while let Some(bundle) = self.allocation_queue.pop() {
            self.stats.process_bundle_count += 1;
            self.process_bundle(bundle);
            count += 1;
            if count > self.func.insts() * 50 {
                self.dump_state();
                panic!("Infinite loop!");
            }
        }
        self.stats.final_liverange_count = self.ranges.len();
        self.stats.final_bundle_count = self.bundles.len();
        self.stats.spill_bundle_count = self.spilled_bundles.len();
    }

    fn dump_state(&self) {
        log::debug!("Bundles:");
        for (i, b) in self.bundles.iter().enumerate() {
            log::debug!(
                "bundle{}: first_range={:?} last_range={:?} spillset={:?} alloc={:?}",
                i,
                b.first_range,
                b.last_range,
                b.spillset,
                b.allocation
            );
        }
        log::debug!("VRegs:");
        for (i, v) in self.vregs.iter().enumerate() {
            log::debug!("vreg{}: def={:?} first_range={:?}", i, v.def, v.first_range,);
        }
        log::debug!("Ranges:");
        for (i, r) in self.ranges.iter().enumerate() {
            log::debug!(
                concat!(
                    "range{}: range={:?} vreg={:?} bundle={:?} ",
                    "weight={} fixed={} first_use={:?} last_use={:?} ",
                    "def={:?} next_in_bundle={:?} next_in_reg={:?}"
                ),
                i,
                r.range,
                r.vreg,
                r.bundle,
                r.uses_spill_weight,
                r.num_fixed_uses(),
                r.first_use,
                r.last_use,
                r.def,
                r.next_in_bundle,
                r.next_in_reg
            );
        }
        log::debug!("Uses:");
        for (i, u) in self.uses.iter().enumerate() {
            log::debug!(
                "use{}: op={:?} pos={:?} slot={} next_use={:?}",
                i,
                u.operand,
                u.pos,
                u.slot,
                u.next_use
            );
        }
        log::debug!("Defs:");
        for (i, d) in self.defs.iter().enumerate() {
            log::debug!("def{}: op={:?} pos={:?}", i, d.operand, d.pos,);
        }
    }

    fn compute_requirement(&self, bundle: LiveBundleIndex) -> Option<Requirement> {
        let class = self.vregs[self.ranges[self.bundles[bundle.index()].first_range.index()]
            .vreg
            .index()]
        .reg
        .class();
        let mut needed = Requirement::Any(class);

        log::debug!("compute_requirement: bundle {:?} class {:?}", bundle, class);

        let mut iter = self.bundles[bundle.index()].first_range;
        while iter.is_valid() {
            let range = &self.ranges[iter.index()];
            log::debug!(" -> range {:?}", range.range);
            if range.def.is_valid() {
                let def_op = self.defs[range.def.index()].operand;
                let def_req = Requirement::from_operand(def_op);
                log::debug!(
                    " -> def {:?} op {:?} req {:?}",
                    range.def.index(),
                    def_op,
                    def_req
                );
                needed = needed.merge(def_req)?;
                log::debug!("   -> needed {:?}", needed);
            }
            let mut use_iter = range.first_use;
            while use_iter.is_valid() {
                let usedata = &self.uses[use_iter.index()];
                let use_op = usedata.operand;
                let use_req = Requirement::from_operand(use_op);
                log::debug!(" -> use {:?} op {:?} req {:?}", use_iter, use_op, use_req);
                needed = needed.merge(use_req)?;
                log::debug!("   -> needed {:?}", needed);
                use_iter = usedata.next_use;
            }
            iter = range.next_in_bundle;
        }

        log::debug!(" -> final needed: {:?}", needed);
        Some(needed)
    }

    fn try_to_allocate_bundle_to_reg(
        &mut self,
        bundle: LiveBundleIndex,
        reg: PRegIndex,
    ) -> AllocRegResult {
        log::debug!("try_to_allocate_bundle_to_reg: {:?} -> {:?}", bundle, reg);
        let mut conflicts = smallvec![];
        let mut iter = self.bundles[bundle.index()].first_range;
        while iter.is_valid() {
            let range = &self.ranges[iter.index()];
            log::debug!(" -> range {:?}", range);
            // Note that the comparator function here tests for *overlap*, so we
            // are checking whether the BTree contains any preg range that
            // *overlaps* with range `iter`, not literally the range `iter`.
            if let Some(preg_range) = self.pregs[reg.index()]
                .allocations
                .btree
                .get(&LiveRangeKey::from_range(&range.range))
            {
                log::debug!(" -> btree contains range {:?} that overlaps", preg_range);
                if self.ranges[preg_range.index()].vreg.is_valid() {
                    log::debug!("   -> from vreg {:?}", self.ranges[preg_range.index()].vreg);
                    // range from an allocated bundle: find the bundle and add to
                    // conflicts list.
                    let conflict_bundle = self.ranges[preg_range.index()].bundle;
                    log::debug!("   -> conflict bundle {:?}", conflict_bundle);
                    if !conflicts.iter().any(|b| *b == conflict_bundle) {
                        conflicts.push(conflict_bundle);
                    }
                } else {
                    log::debug!("   -> conflict with fixed reservation");
                    // range from a direct use of the PReg (due to clobber).
                    return AllocRegResult::ConflictWithFixed;
                }
            }
            iter = range.next_in_bundle;
        }

        if conflicts.len() > 0 {
            return AllocRegResult::Conflict(conflicts);
        }

        // We can allocate! Add our ranges to the preg's BTree.
        let preg = self.pregs[reg.index()].reg;
        log::debug!("  -> bundle {:?} assigned to preg {:?}", bundle, preg);
        self.bundles[bundle.index()].allocation = Allocation::reg(preg);
        let mut iter = self.bundles[bundle.index()].first_range;
        while iter.is_valid() {
            let range = &self.ranges[iter.index()];
            self.pregs[reg.index()]
                .allocations
                .btree
                .insert(LiveRangeKey::from_range(&range.range), iter);
            iter = range.next_in_bundle;
        }

        AllocRegResult::Allocated(Allocation::reg(preg))
    }

    fn evict_bundle(&mut self, bundle: LiveBundleIndex) {
        log::debug!(
            "evicting bundle {:?}: alloc {:?}",
            bundle,
            self.bundles[bundle.index()].allocation
        );
        let preg = match self.bundles[bundle.index()].allocation.as_reg() {
            Some(preg) => preg,
            None => {
                log::debug!(
                    "  -> has no allocation! {:?}",
                    self.bundles[bundle.index()].allocation
                );
                return;
            }
        };
        let preg_idx = PRegIndex::new(preg.index());
        self.bundles[bundle.index()].allocation = Allocation::none();
        let mut iter = self.bundles[bundle.index()].first_range;
        while iter.is_valid() {
            log::debug!(" -> removing LR {:?} from reg {:?}", iter, preg_idx);
            self.pregs[preg_idx.index()]
                .allocations
                .btree
                .remove(&LiveRangeKey::from_range(&self.ranges[iter.index()].range));
            iter = self.ranges[iter.index()].next_in_bundle;
        }
        let prio = self.bundles[bundle.index()].prio;
        log::debug!(" -> prio {}; back into queue", prio);
        self.allocation_queue.insert(bundle, prio as usize);
    }

    fn bundle_spill_weight(&self, bundle: LiveBundleIndex) -> u32 {
        self.bundles[bundle.index()].cached_spill_weight()
    }

    fn maximum_spill_weight_in_bundle_set(&self, bundles: &LiveBundleVec) -> u32 {
        bundles
            .iter()
            .map(|&b| self.bundles[b.index()].cached_spill_weight())
            .max()
            .unwrap_or(0)
    }

    fn recompute_bundle_properties(&mut self, bundle: LiveBundleIndex) {
        let minimal;
        let mut fixed = false;
        let bundledata = &self.bundles[bundle.index()];
        let first_range = &self.ranges[bundledata.first_range.index()];

        if first_range.vreg.is_invalid() {
            minimal = true;
            fixed = true;
        } else {
            if first_range.def.is_valid() {
                let def_data = &self.defs[first_range.def.index()];
                if let OperandPolicy::FixedReg(_) = def_data.operand.policy() {
                    fixed = true;
                }
            }
            let mut use_iter = first_range.first_use;
            while use_iter.is_valid() {
                let use_data = &self.uses[use_iter.index()];
                if let OperandPolicy::FixedReg(_) = use_data.operand.policy() {
                    fixed = true;
                    break;
                }
                use_iter = use_data.next_use;
            }
            // Minimal if this is the only range in the bundle, and if
            // the range covers only one instruction. Note that it
            // could cover just one ProgPoint, i.e. X.Before..X.After,
            // or two ProgPoints, i.e. X.Before..X+1.Before.
            minimal = first_range.next_in_bundle.is_invalid()
                && first_range.range.from.inst == first_range.range.to.prev().inst;
        }

        let spill_weight = if minimal {
            if fixed {
                log::debug!("  -> fixed and minimal: 2000000");
                2_000_000
            } else {
                log::debug!("  -> non-fixed and minimal: 1000000");
                1_000_000
            }
        } else {
            let mut total = 0;
            let mut range = self.bundles[bundle.index()].first_range;
            while range.is_valid() {
                let range_data = &self.ranges[range.index()];
                if range_data.def.is_valid() {
                    log::debug!("  -> has def (2000)");
                    total += 2000;
                }
                log::debug!("  -> uses spill weight: {}", range_data.uses_spill_weight);
                total += range_data.uses_spill_weight;
                range = range_data.next_in_bundle;
            }

            if self.bundles[bundle.index()].prio > 0 {
                total / self.bundles[bundle.index()].prio
            } else {
                total
            }
        };

        self.bundles[bundle.index()].set_cached_spill_weight_and_props(
            spill_weight,
            minimal,
            fixed,
        );
    }

    fn minimal_bundle(&mut self, bundle: LiveBundleIndex) -> bool {
        self.bundles[bundle.index()].cached_minimal()
    }

    fn find_split_points(
        &mut self,
        bundle: LiveBundleIndex,
        conflicting: LiveBundleIndex,
    ) -> SmallVec<[ProgPoint; 4]> {
        // Scan the bundle's ranges once. We want to record:
        // - Does the bundle contain any ranges in "hot" code and/or "cold" code?
        //   If so, record the transition points that are fully included in
        //   `bundle`: the first ProgPoint in a hot range if the prior cold
        //   point is also in the bundle; and the first ProgPoint in a cold
        //   range if the prior hot point is also in the bundle.
        // - Does the bundle cross any clobbering insts?
        //   If so, record the ProgPoint before each such instruction.
        // - Is there a register use before the conflicting bundle?
        //   If so, record the ProgPoint just after the last one.
        // - Is there a register use after the conflicting bundle?
        //   If so, record the ProgPoint just before the last one.
        //
        // Then choose one of the above kinds of splits, in priority order.

        let mut cold_hot_splits: SmallVec<[ProgPoint; 4]> = smallvec![];
        let mut clobber_splits: SmallVec<[ProgPoint; 4]> = smallvec![];
        let mut last_before_conflict: Option<ProgPoint> = None;
        let mut first_after_conflict: Option<ProgPoint> = None;

        log::debug!(
            "find_split_points: bundle {:?} conflicting {:?}",
            bundle,
            conflicting
        );

        // We simultaneously scan the sorted list of LiveRanges in our bundle
        // and the sorted list of call instruction locations. We also take the
        // total range (start of first range to end of last range) of the
        // conflicting bundle, if any, so we can find the last use before it and
        // first use after it. Each loop iteration handles one range in our
        // bundle. Calls are scanned up until they advance past the current
        // range.
        let mut our_iter = self.bundles[bundle.index()].first_range;
        let (conflict_from, conflict_to) = if conflicting.is_valid() {
            (
                Some(
                    self.ranges[self.bundles[conflicting.index()].first_range.index()]
                        .range
                        .from,
                ),
                Some(
                    self.ranges[self.bundles[conflicting.index()].last_range.index()]
                        .range
                        .to,
                ),
            )
        } else {
            (None, None)
        };

        let bundle_start = if self.bundles[bundle.index()].first_range.is_valid() {
            self.ranges[self.bundles[bundle.index()].first_range.index()]
                .range
                .from
        } else {
            ProgPoint::before(Inst::new(0))
        };
        let bundle_end = if self.bundles[bundle.index()].last_range.is_valid() {
            self.ranges[self.bundles[bundle.index()].last_range.index()]
                .range
                .to
        } else {
            ProgPoint::before(Inst::new(self.func.insts()))
        };

        log::debug!(" -> conflict from {:?} to {:?}", conflict_from, conflict_to);
        let mut clobberidx = 0;
        while our_iter.is_valid() {
            // Probe the hot-code tree.
            let our_range = self.ranges[our_iter.index()].range;
            log::debug!(" -> range {:?}", our_range);
            if let Some(hot_range_idx) = self
                .hot_code
                .btree
                .get(&LiveRangeKey::from_range(&our_range))
            {
                // `hot_range_idx` is a range that *overlaps* with our range.

                // There may be cold code in our range on either side of the hot
                // range. Record the transition points if so.
                let hot_range = self.ranges[hot_range_idx.index()].range;
                log::debug!("   -> overlaps with hot-code range {:?}", hot_range);
                let start_cold = our_range.from < hot_range.from;
                let end_cold = our_range.to > hot_range.to;
                if start_cold {
                    log::debug!(
                        "    -> our start is cold; potential split at cold->hot transition {:?}",
                        hot_range.from,
                    );
                    // First ProgPoint in hot range.
                    cold_hot_splits.push(hot_range.from);
                }
                if end_cold {
                    log::debug!(
                        "    -> our end is cold; potential split at hot->cold transition {:?}",
                        hot_range.to,
                    );
                    // First ProgPoint in cold range (after hot range).
                    cold_hot_splits.push(hot_range.to);
                }
            }

            // Scan through clobber-insts from last left-off position until the first
            // clobbering inst past this range. Record all clobber sites as potential
            // splits.
            while clobberidx < self.clobbers.len() {
                let cur_clobber = self.clobbers[clobberidx];
                let pos = ProgPoint::before(cur_clobber);
                if pos >= our_range.to {
                    break;
                }
                clobberidx += 1;
                if pos < our_range.from {
                    continue;
                }
                if pos > bundle_start {
                    log::debug!("   -> potential clobber split at {:?}", pos);
                    clobber_splits.push(pos);
                }
            }

            // Update last-before-conflict and first-before-conflict positions.

            let mut update_with_pos = |pos: ProgPoint| {
                let before_inst = ProgPoint::before(pos.inst);
                let before_next_inst = before_inst.next().next();
                if before_inst > bundle_start
                    && (conflict_from.is_none() || before_inst < conflict_from.unwrap())
                    && (last_before_conflict.is_none()
                        || before_inst > last_before_conflict.unwrap())
                {
                    last_before_conflict = Some(before_inst);
                }
                if before_next_inst < bundle_end
                    && (conflict_to.is_none() || pos >= conflict_to.unwrap())
                    && (first_after_conflict.is_none() || pos > first_after_conflict.unwrap())
                {
                    first_after_conflict = Some(ProgPoint::before(pos.inst.next()));
                }
            };

            if self.ranges[our_iter.index()].def.is_valid() {
                let def_data = &self.defs[self.ranges[our_iter.index()].def.index()];
                log::debug!("   -> range has def at {:?}", def_data.pos);
                update_with_pos(def_data.pos);
            }
            let mut use_idx = self.ranges[our_iter.index()].first_use;
            while use_idx.is_valid() {
                let use_data = &self.uses[use_idx.index()];
                log::debug!("   -> range has use at {:?}", use_data.pos);
                update_with_pos(use_data.pos);
                use_idx = use_data.next_use;
            }

            our_iter = self.ranges[our_iter.index()].next_in_bundle;
        }
        log::debug!(
            "  -> first use/def after conflict range: {:?}",
            first_after_conflict,
        );
        log::debug!(
            "  -> last use/def before conflict range: {:?}",
            last_before_conflict,
        );

        // Based on the above, we can determine which split strategy we are taking at this
        // iteration:
        // - If we span both hot and cold code, split into separate "hot" and "cold" bundles.
        // - Otherwise, if we span any calls, split just before every call instruction.
        // - Otherwise, if there is a register use after the conflicting bundle,
        //   split at that use-point ("split before first use").
        // - Otherwise, if there is a register use before the conflicting
        //   bundle, split at that use-point ("split after last use").
        // - Otherwise, split at every use, to form minimal bundles.

        if cold_hot_splits.len() > 0 {
            log::debug!(" going with cold/hot splits: {:?}", cold_hot_splits);
            self.stats.splits_hot += 1;
            cold_hot_splits
        } else if clobber_splits.len() > 0 {
            log::debug!(" going with clobber splits: {:?}", clobber_splits);
            self.stats.splits_clobbers += 1;
            clobber_splits
        } else if first_after_conflict.is_some() {
            self.stats.splits_conflicts += 1;
            log::debug!(" going with first after conflict");
            smallvec![first_after_conflict.unwrap()]
        } else if last_before_conflict.is_some() {
            self.stats.splits_conflicts += 1;
            log::debug!(" going with last before conflict");
            smallvec![last_before_conflict.unwrap()]
        } else {
            self.stats.splits_all += 1;
            log::debug!(" splitting at all uses");
            self.find_all_use_split_points(bundle)
        }
    }

    fn find_all_use_split_points(&self, bundle: LiveBundleIndex) -> SmallVec<[ProgPoint; 4]> {
        let mut splits = smallvec![];
        let mut iter = self.bundles[bundle.index()].first_range;
        log::debug!("finding all use/def splits for {:?}", bundle);
        let (bundle_start, bundle_end) = if iter.is_valid() {
            (
                self.ranges[iter.index()].range.from,
                self.ranges[self.bundles[bundle.index()].last_range.index()]
                    .range
                    .to,
            )
        } else {
            (
                ProgPoint::before(Inst::new(0)),
                ProgPoint::after(Inst::new(self.func.insts() - 1)),
            )
        };
        // N.B.: a minimal bundle must include only ProgPoints in a
        // single instruction, but can include both (can include two
        // ProgPoints). We split here, taking care to never split *in
        // the middle* of an instruction, because we would not be able
        // to insert moves to reify such an assignment.
        while iter.is_valid() {
            let rangedata = &self.ranges[iter.index()];
            log::debug!(" -> range {:?}: {:?}", iter, rangedata.range);
            if rangedata.def.is_valid() {
                // Split both before and after def (make it a minimal bundle).
                let def_pos = self.defs[rangedata.def.index()].pos;
                let def_end = ProgPoint::before(def_pos.inst.next());
                log::debug!(
                    "  -> splitting before and after def: {:?} and {:?}",
                    def_pos,
                    def_end,
                );
                if def_pos > bundle_start {
                    splits.push(def_pos);
                }
                if def_end < bundle_end {
                    splits.push(def_end);
                }
            }
            let mut use_idx = rangedata.first_use;
            while use_idx.is_valid() {
                let use_data = &self.uses[use_idx.index()];
                let before_use_inst = ProgPoint::before(use_data.pos.inst);
                let after_use_inst = before_use_inst.next().next();
                log::debug!(
                    "  -> splitting before and after use: {:?} and {:?}",
                    before_use_inst,
                    after_use_inst,
                );
                if before_use_inst > bundle_start {
                    splits.push(before_use_inst);
                }
                splits.push(after_use_inst);
                use_idx = use_data.next_use;
            }

            iter = rangedata.next_in_bundle;
        }
        splits.sort();
        log::debug!(" -> final splits: {:?}", splits);
        splits
    }

    fn split_and_requeue_bundle(
        &mut self,
        bundle: LiveBundleIndex,
        first_conflicting_bundle: LiveBundleIndex,
    ) {
        self.stats.splits += 1;
        // Try splitting: (i) across hot code; (ii) across all calls,
        // if we had a fixed-reg conflict; (iii) before first reg use;
        // (iv) after reg use; (v) around all register uses.  After
        // each type of split, check for conflict with conflicting
        // bundle(s); stop when no conflicts. In all cases, re-queue
        // the split bundles on the allocation queue.
        //
        // The critical property here is that we must eventually split
        // down to minimal bundles, which consist just of live ranges
        // around each individual def/use (this is step (v)
        // above). This ensures termination eventually.

        let split_points = self.find_split_points(bundle, first_conflicting_bundle);
        log::debug!(
            "split bundle {:?} (conflict {:?}): split points {:?}",
            bundle,
            first_conflicting_bundle,
            split_points
        );

        // Split `bundle` at every ProgPoint in `split_points`,
        // creating new LiveRanges and bundles (and updating vregs'
        // linked lists appropriately), and enqueue the new bundles.
        //
        // We uphold several basic invariants here:
        // - The LiveRanges in every vreg, and in every bundle, are disjoint
        // - Every bundle for a given vreg is disjoint
        //
        // To do so, we make one scan in program order: all ranges in
        // the bundle, and the def/all uses in each range. We track
        // the currently active bundle. For each range, we distribute
        // its uses among one or more ranges, depending on whether it
        // crosses any split points. If we had to split a range, then
        // we need to insert the new subparts in its vreg as
        // well. N.B.: to avoid the need to *remove* ranges from vregs
        // (which we could not do without a lookup, since we use
        // singly-linked lists and the bundle may contain multiple
        // vregs so we cannot simply scan a single vreg simultaneously
        // to the main scan), we instead *trim* the existing range
        // into its first subpart, and then create the new
        // subparts. Note that shrinking a LiveRange is always legal
        // (as long as one replaces the shrunk space with new
        // LiveRanges).
        //
        // Note that the original IonMonkey splitting code is quite a
        // bit more complex and has some subtle invariants. We stick
        // to the above invariants to keep this code maintainable.

        let mut split_idx = 0;

        // Fast-forward past any splits that occur before or exactly
        // at the start of the first range in the bundle.
        let first_range = self.bundles[bundle.index()].first_range;
        let bundle_start = if first_range.is_valid() {
            self.ranges[first_range.index()].range.from
        } else {
            ProgPoint::before(Inst::new(0))
        };
        while split_idx < split_points.len() && split_points[split_idx] <= bundle_start {
            split_idx += 1;
        }

        let mut new_bundles: LiveBundleVec = smallvec![];
        let mut cur_bundle = bundle;
        let mut iter = self.bundles[bundle.index()].first_range;
        self.bundles[bundle.index()].first_range = LiveRangeIndex::invalid();
        self.bundles[bundle.index()].last_range = LiveRangeIndex::invalid();
        while iter.is_valid() {
            // Read `next` link now and then clear it -- we rebuild the list below.
            let next = self.ranges[iter.index()].next_in_bundle;
            self.ranges[iter.index()].next_in_bundle = LiveRangeIndex::invalid();

            let mut range = self.ranges[iter.index()].range;
            log::debug!(" -> has range {:?} (LR {:?})", range, iter);

            // If any splits occur before this range, create a new
            // bundle, then advance to the first split within the
            // range.
            if split_idx < split_points.len() && split_points[split_idx] <= range.from {
                log::debug!("  -> split before a range; creating new bundle");
                cur_bundle = self.create_bundle();
                self.bundles[cur_bundle.index()].spillset = self.bundles[bundle.index()].spillset;
                new_bundles.push(cur_bundle);
                split_idx += 1;
            }
            while split_idx < split_points.len() && split_points[split_idx] <= range.from {
                split_idx += 1;
            }

            // Link into current bundle.
            self.ranges[iter.index()].bundle = cur_bundle;
            if self.bundles[cur_bundle.index()].first_range.is_valid() {
                self.ranges[self.bundles[cur_bundle.index()].last_range.index()].next_in_bundle =
                    iter;
            } else {
                self.bundles[cur_bundle.index()].first_range = iter;
            }
            self.bundles[cur_bundle.index()].last_range = iter;

            // While the next split point is beyond the start of the
            // range and before the end, shorten the current LiveRange
            // (this is always legal) and create a new Bundle and
            // LiveRange for the remainder. Truncate the old bundle
            // (set last_range). Insert the LiveRange into the vreg
            // and into the new bundle. Then move the use-chain over,
            // splitting at the appropriate point.
            //
            // We accumulate the use stats (fixed-use count and spill
            // weight) as we scan through uses, recomputing the values
            // for the truncated initial LiveRange and taking the
            // remainders for the split "rest" LiveRange.

            while split_idx < split_points.len() && split_points[split_idx] < range.to {
                let split_point = split_points[split_idx];
                split_idx += 1;

                // Skip forward to the current range.
                if split_point <= range.from {
                    continue;
                }

                log::debug!(
                    " -> processing split point {:?} with iter {:?}",
                    split_point,
                    iter
                );

                // We split into `first` and `rest`. `rest` may be
                // further subdivided in subsequent iterations; we
                // only do one split per iteration.
                debug_assert!(range.from < split_point && split_point < range.to);
                let rest_range = CodeRange {
                    from: split_point,
                    to: self.ranges[iter.index()].range.to,
                };
                self.ranges[iter.index()].range.to = split_point;
                range = rest_range;
                log::debug!(
                    " -> range of {:?} now {:?}",
                    iter,
                    self.ranges[iter.index()].range
                );

                // Create the rest-range and insert it into the vreg's
                // range list. (Note that the vreg does not keep a
                // tail-pointer so we do not need to update that.)
                let rest_lr = self.create_liverange(rest_range);
                self.ranges[rest_lr.index()].vreg = self.ranges[iter.index()].vreg;
                self.ranges[rest_lr.index()].next_in_reg = self.ranges[iter.index()].next_in_reg;
                self.ranges[iter.index()].next_in_reg = rest_lr;

                log::debug!(
                    " -> split tail to new LR {:?} with range {:?}",
                    rest_lr,
                    rest_range
                );

                // Scan over uses, accumulating stats for those that
                // stay in the first range, finding the first use that
                // moves to the rest range.
                let mut last_use_in_first_range = UseIndex::invalid();
                let mut use_iter = self.ranges[iter.index()].first_use;
                let mut num_fixed_uses = 0;
                let mut uses_spill_weight = 0;
                while use_iter.is_valid() {
                    if self.uses[use_iter.index()].pos >= split_point {
                        break;
                    }
                    last_use_in_first_range = use_iter;
                    let policy = self.uses[use_iter.index()].operand.policy();
                    log::debug!(
                        " -> use {:?} before split point; policy {:?}",
                        use_iter,
                        policy
                    );
                    if let OperandPolicy::FixedReg(_) = policy {
                        num_fixed_uses += 1;
                    }
                    uses_spill_weight += spill_weight_from_policy(policy);
                    log::debug!("   -> use {:?} remains in orig", use_iter);
                    use_iter = self.uses[use_iter.index()].next_use;
                }

                // Move over `rest`'s uses and update stats on first
                // and rest LRs.
                if use_iter.is_valid() {
                    log::debug!(
                        "   -> moving uses over the split starting at {:?}",
                        use_iter
                    );
                    self.ranges[rest_lr.index()].first_use = use_iter;
                    self.ranges[rest_lr.index()].last_use = self.ranges[iter.index()].last_use;

                    self.ranges[iter.index()].last_use = last_use_in_first_range;
                    if last_use_in_first_range.is_valid() {
                        self.uses[last_use_in_first_range.index()].next_use = UseIndex::invalid();
                    } else {
                        self.ranges[iter.index()].first_use = UseIndex::invalid();
                    }

                    let rest_fixed_uses =
                        self.ranges[iter.index()].num_fixed_uses() - num_fixed_uses;
                    self.ranges[rest_lr.index()].set_num_fixed_uses(rest_fixed_uses);
                    self.ranges[rest_lr.index()].uses_spill_weight =
                        self.ranges[iter.index()].uses_spill_weight - uses_spill_weight;
                    self.ranges[iter.index()].set_num_fixed_uses(num_fixed_uses);
                    self.ranges[iter.index()].uses_spill_weight = uses_spill_weight;
                }

                // Move over def, if appropriate.
                if self.ranges[iter.index()].def.is_valid() {
                    let def_idx = self.ranges[iter.index()].def;
                    let def_pos = self.defs[def_idx.index()].pos;
                    log::debug!(" -> range {:?} has def at {:?}", iter, def_pos);
                    if def_pos >= split_point {
                        log::debug!(" -> transferring def bit to {:?}", rest_lr);
                        self.ranges[iter.index()].def = DefIndex::invalid();
                        self.ranges[rest_lr.index()].def = def_idx;
                    }
                }

                log::debug!(
                    " -> range {:?} next-in-bundle is {:?}",
                    iter,
                    self.ranges[iter.index()].next_in_bundle
                );

                // Create a new bundle to hold the rest-range.
                let rest_bundle = self.create_bundle();
                cur_bundle = rest_bundle;
                new_bundles.push(rest_bundle);
                self.bundles[rest_bundle.index()].first_range = rest_lr;
                self.bundles[rest_bundle.index()].last_range = rest_lr;
                self.bundles[rest_bundle.index()].spillset = self.bundles[bundle.index()].spillset;
                self.ranges[rest_lr.index()].bundle = rest_bundle;
                log::debug!(" -> new bundle {:?} for LR {:?}", rest_bundle, rest_lr);

                iter = rest_lr;
            }

            iter = next;
        }

        // Enqueue all split-bundles on the allocation queue.
        let prio = self.compute_bundle_prio(bundle);
        self.bundles[bundle.index()].prio = prio;
        self.recompute_bundle_properties(bundle);
        self.allocation_queue.insert(bundle, prio as usize);
        for b in new_bundles {
            let prio = self.compute_bundle_prio(b);
            self.bundles[b.index()].prio = prio;
            self.recompute_bundle_properties(b);
            self.allocation_queue.insert(b, prio as usize);
        }
    }

    fn process_bundle(&mut self, bundle: LiveBundleIndex) {
        // Find any requirements: for every LR, for every def/use, gather
        // requirements (fixed-reg, any-reg, any) and merge them.
        let req = self.compute_requirement(bundle);
        // Grab a hint from our spillset, if any.
        let hint_reg = self.spillsets[self.bundles[bundle.index()].spillset.index()].reg_hint;
        log::debug!(
            "process_bundle: bundle {:?} requirement {:?} hint {:?}",
            bundle,
            req,
            hint_reg,
        );

        // Try to allocate!
        let mut attempts = 0;
        let mut first_conflicting_bundle;
        loop {
            attempts += 1;
            debug_assert!(attempts < 100 * self.func.insts());
            first_conflicting_bundle = None;
            let req = match req {
                Some(r) => r,
                // `None` means conflicting requirements, hence impossible to
                // allocate.
                None => break,
            };

            let conflicting_bundles = match req {
                Requirement::Fixed(preg) => {
                    let preg_idx = PRegIndex::new(preg.index());
                    self.stats.process_bundle_reg_probes_fixed += 1;
                    match self.try_to_allocate_bundle_to_reg(bundle, preg_idx) {
                        AllocRegResult::Allocated(alloc) => {
                            self.stats.process_bundle_reg_success_fixed += 1;
                            log::debug!(" -> allocated to fixed {:?}", preg_idx);
                            self.spillsets[self.bundles[bundle.index()].spillset.index()]
                                .reg_hint = Some(alloc.as_reg().unwrap());
                            return;
                        }
                        AllocRegResult::Conflict(bundles) => bundles,
                        AllocRegResult::ConflictWithFixed => {
                            // Empty conflicts set: there's nothing we can
                            // evict, because fixed conflicts cannot be moved.
                            smallvec![]
                        }
                    }
                }
                Requirement::Register(class) => {
                    // Scan all pregs and attempt to allocate.
                    let mut lowest_cost_conflict_set: Option<LiveBundleVec> = None;
                    let n_regs = self.env.regs_by_class[class as u8 as usize].len();
                    let loop_count = if hint_reg.is_some() {
                        n_regs + 1
                    } else {
                        n_regs
                    };
                    for i in 0..loop_count {
                        // The order in which we try registers is somewhat complex:
                        // - First, if there is a hint, we try that.
                        // - Then, we try registers in a traversal
                        //   order that is based on the bundle index,
                        //   spreading pressure evenly among registers
                        //   to reduce commitment-map
                        //   contention. (TODO: account for
                        //   caller-save vs. callee-saves here too.)
                        //   Note that we avoid retrying the hint_reg;
                        //   this is why the loop count is n_regs + 1
                        //   if there is a hint reg, because we always
                        //   skip one iteration.
                        let preg = match (i, hint_reg) {
                            (0, Some(hint_reg)) => hint_reg,
                            (i, Some(hint_reg)) => {
                                let reg = self.env.regs_by_class[class as u8 as usize]
                                    [(i - 1 + bundle.index()) % n_regs];
                                if reg == hint_reg {
                                    continue;
                                }
                                reg
                            }
                            (i, None) => {
                                self.env.regs_by_class[class as u8 as usize]
                                    [(i + bundle.index()) % n_regs]
                            }
                        };

                        self.stats.process_bundle_reg_probes_any += 1;
                        let preg_idx = PRegIndex::new(preg.index());
                        match self.try_to_allocate_bundle_to_reg(bundle, preg_idx) {
                            AllocRegResult::Allocated(alloc) => {
                                self.stats.process_bundle_reg_success_any += 1;
                                log::debug!(" -> allocated to any {:?}", preg_idx);
                                self.spillsets[self.bundles[bundle.index()].spillset.index()]
                                    .reg_hint = Some(alloc.as_reg().unwrap());
                                return;
                            }
                            AllocRegResult::Conflict(bundles) => {
                                if lowest_cost_conflict_set.is_none() {
                                    lowest_cost_conflict_set = Some(bundles);
                                } else if self.maximum_spill_weight_in_bundle_set(&bundles)
                                    < self.maximum_spill_weight_in_bundle_set(
                                        lowest_cost_conflict_set.as_ref().unwrap(),
                                    )
                                {
                                    lowest_cost_conflict_set = Some(bundles);
                                }
                            }
                            AllocRegResult::ConflictWithFixed => {
                                // Simply don't consider as an option.
                            }
                        }
                    }

                    // Otherwise, we *require* a register, but didn't fit into
                    // any with current bundle assignments. Hence, we will need
                    // to either split or attempt to evict some bundles. Return
                    // the conflicting bundles to evict and retry. Empty list
                    // means nothing to try (due to fixed conflict) so we must
                    // split instead.
                    lowest_cost_conflict_set.unwrap_or(smallvec![])
                }

                Requirement::Any(_) => {
                    // If a register is not *required*, spill now (we'll retry
                    // allocation on spilled bundles later).
                    log::debug!("spilling bundle {:?} to spilled_bundles list", bundle);
                    self.spilled_bundles.push(bundle);
                    return;
                }
            };

            log::debug!(" -> conflict set {:?}", conflicting_bundles);

            // If we have already tried evictions once before and are still unsuccessful, give up
            // and move on to splitting as long as this is not a minimal bundle.
            if attempts >= 2 && !self.minimal_bundle(bundle) {
                break;
            }

            // If we hit a fixed conflict, give up and move on to splitting.
            if conflicting_bundles.is_empty() {
                break;
            }

            first_conflicting_bundle = Some(conflicting_bundles[0]);

            // If the maximum spill weight in the conflicting-bundles set is >= this bundle's spill
            // weight, then don't evict.
            if self.maximum_spill_weight_in_bundle_set(&conflicting_bundles)
                >= self.bundle_spill_weight(bundle)
            {
                log::debug!(" -> we're already the cheapest bundle to spill -- going to split");
                break;
            }

            // Evict all bundles in `conflicting bundles` and try again.
            self.stats.evict_bundle_event += 1;
            for &bundle in &conflicting_bundles {
                log::debug!(" -> evicting {:?}", bundle);
                self.evict_bundle(bundle);
                self.stats.evict_bundle_count += 1;
            }
        }

        // A minimal bundle cannot be split.
        if self.minimal_bundle(bundle) {
            self.dump_state();
        }
        debug_assert!(!self.minimal_bundle(bundle));

        self.split_and_requeue_bundle(
            bundle,
            first_conflicting_bundle.unwrap_or(LiveBundleIndex::invalid()),
        );
    }

    fn try_allocating_regs_for_spilled_bundles(&mut self) {
        for i in 0..self.spilled_bundles.len() {
            let bundle = self.spilled_bundles[i]; // don't borrow self
            let any_vreg = self.vregs[self.ranges
                [self.bundles[bundle.index()].first_range.index()]
            .vreg
            .index()]
            .reg;
            let class = any_vreg.class();
            let mut success = false;
            self.stats.spill_bundle_reg_probes += 1;
            let nregs = self.env.regs_by_class[class as u8 as usize].len();
            for i in 0..nregs {
                let i = (i + bundle.index()) % nregs;
                let preg = self.env.regs_by_class[class as u8 as usize][i]; // don't borrow self
                let preg_idx = PRegIndex::new(preg.index());
                if let AllocRegResult::Allocated(_) =
                    self.try_to_allocate_bundle_to_reg(bundle, preg_idx)
                {
                    self.stats.spill_bundle_reg_success += 1;
                    success = true;
                    break;
                }
            }
            if !success {
                log::debug!(
                    "spilling bundle {:?} to spillset bundle list {:?}",
                    bundle,
                    self.bundles[bundle.index()].spillset
                );
                self.spillsets[self.bundles[bundle.index()].spillset.index()]
                    .bundles
                    .push(bundle);
            }
        }
    }

    fn spillslot_can_fit_spillset(
        &mut self,
        spillslot: SpillSlotIndex,
        spillset: SpillSetIndex,
    ) -> bool {
        for &bundle in &self.spillsets[spillset.index()].bundles {
            let mut iter = self.bundles[bundle.index()].first_range;
            while iter.is_valid() {
                let range = self.ranges[iter.index()].range;
                if self.spillslots[spillslot.index()]
                    .ranges
                    .btree
                    .contains_key(&LiveRangeKey::from_range(&range))
                {
                    return false;
                }
                iter = self.ranges[iter.index()].next_in_bundle;
            }
        }
        true
    }

    fn allocate_spillset_to_spillslot(
        &mut self,
        spillset: SpillSetIndex,
        spillslot: SpillSlotIndex,
    ) {
        self.spillsets[spillset.index()].slot = spillslot;
        for i in 0..self.spillsets[spillset.index()].bundles.len() {
            // don't borrow self
            let bundle = self.spillsets[spillset.index()].bundles[i];
            log::debug!(
                "spillslot {:?} alloc'ed to spillset {:?}: bundle {:?}",
                spillslot,
                spillset,
                bundle
            );
            let mut iter = self.bundles[bundle.index()].first_range;
            while iter.is_valid() {
                log::debug!(
                    "spillslot {:?} getting range {:?} from bundle {:?}: {:?}",
                    spillslot,
                    iter,
                    bundle,
                    self.ranges[iter.index()].range
                );
                let range = self.ranges[iter.index()].range;
                self.spillslots[spillslot.index()]
                    .ranges
                    .btree
                    .insert(LiveRangeKey::from_range(&range), iter);
                iter = self.ranges[iter.index()].next_in_bundle;
            }
        }
    }

    fn allocate_spillslots(&mut self) {
        for spillset in 0..self.spillsets.len() {
            log::debug!("allocate spillslot: {}", spillset);
            let spillset = SpillSetIndex::new(spillset);
            if self.spillsets[spillset.index()].bundles.is_empty() {
                continue;
            }
            // Get or create the spillslot list for this size.
            let size = self.spillsets[spillset.index()].size as usize;
            if size >= self.slots_by_size.len() {
                self.slots_by_size.resize(
                    size + 1,
                    SpillSlotList {
                        first_spillslot: SpillSlotIndex::invalid(),
                        last_spillslot: SpillSlotIndex::invalid(),
                    },
                );
            }
            // Try a few existing spillslots.
            let mut spillslot_iter = self.slots_by_size[size].first_spillslot;
            let mut first_slot = SpillSlotIndex::invalid();
            let mut prev = SpillSlotIndex::invalid();
            let mut success = false;
            for _attempt in 0..10 {
                if spillslot_iter.is_invalid() {
                    break;
                }
                if spillslot_iter == first_slot {
                    // We've started looking at slots we placed at the end; end search.
                    break;
                }
                if first_slot.is_invalid() {
                    first_slot = spillslot_iter;
                }

                if self.spillslot_can_fit_spillset(spillslot_iter, spillset) {
                    self.allocate_spillset_to_spillslot(spillset, spillslot_iter);
                    success = true;
                    break;
                }
                // Remove the slot and place it at the end of the respective list.
                let next = self.spillslots[spillslot_iter.index()].next_spillslot;
                if prev.is_valid() {
                    self.spillslots[prev.index()].next_spillslot = next;
                } else {
                    self.slots_by_size[size].first_spillslot = next;
                }
                if !next.is_valid() {
                    self.slots_by_size[size].last_spillslot = prev;
                }

                let last = self.slots_by_size[size].last_spillslot;
                if last.is_valid() {
                    self.spillslots[last.index()].next_spillslot = spillslot_iter;
                } else {
                    self.slots_by_size[size].first_spillslot = spillslot_iter;
                }
                self.slots_by_size[size].last_spillslot = spillslot_iter;

                prev = spillslot_iter;
                spillslot_iter = next;
            }

            if !success {
                // Allocate a new spillslot.
                let spillslot = SpillSlotIndex::new(self.spillslots.len());
                let next = self.slots_by_size[size].first_spillslot;
                self.spillslots.push(SpillSlotData {
                    ranges: LiveRangeSet::new(),
                    next_spillslot: next,
                    size: size as u32,
                    alloc: Allocation::none(),
                    class: self.spillsets[spillset.index()].class,
                });
                self.slots_by_size[size].first_spillslot = spillslot;
                if !next.is_valid() {
                    self.slots_by_size[size].last_spillslot = spillslot;
                }

                self.allocate_spillset_to_spillslot(spillset, spillslot);
            }
        }

        // Assign actual slot indices to spillslots.
        let mut offset: u32 = 0;
        for data in &mut self.spillslots {
            // Align up to `size`.
            debug_assert!(data.size.is_power_of_two());
            offset = (offset + data.size - 1) & !(data.size - 1);
            let slot = if self.func.multi_spillslot_named_by_last_slot() {
                offset + data.size - 1
            } else {
                offset
            };
            data.alloc = Allocation::stack(SpillSlot::new(slot as usize, data.class));
            offset += data.size;
        }
        self.num_spillslots = offset;

        log::debug!("spillslot allocator done");
    }

    fn is_start_of_block(&self, pos: ProgPoint) -> bool {
        let block = self.cfginfo.insn_block[pos.inst.index()];
        pos == self.cfginfo.block_entry[block.index()]
    }
    fn is_end_of_block(&self, pos: ProgPoint) -> bool {
        let block = self.cfginfo.insn_block[pos.inst.index()];
        pos == self.cfginfo.block_exit[block.index()]
    }

    fn insert_move(
        &mut self,
        pos: ProgPoint,
        prio: InsertMovePrio,
        from_alloc: Allocation,
        to_alloc: Allocation,
    ) {
        debug!(
            "insert_move: pos {:?} prio {:?} from_alloc {:?} to_alloc {:?}",
            pos, prio, from_alloc, to_alloc
        );
        self.inserted_moves.push(InsertedMove {
            pos,
            prio,
            from_alloc,
            to_alloc,
        });
    }

    fn get_alloc(&self, inst: Inst, slot: usize) -> Allocation {
        let inst_allocs = &self.allocs[self.inst_alloc_offsets[inst.index()] as usize..];
        inst_allocs[slot]
    }

    fn set_alloc(&mut self, inst: Inst, slot: usize, alloc: Allocation) {
        let inst_allocs = &mut self.allocs[self.inst_alloc_offsets[inst.index()] as usize..];
        inst_allocs[slot] = alloc;
    }

    fn get_alloc_for_range(&self, range: LiveRangeIndex) -> Allocation {
        let bundledata = &self.bundles[self.ranges[range.index()].bundle.index()];
        if bundledata.allocation != Allocation::none() {
            bundledata.allocation
        } else {
            self.spillslots[self.spillsets[bundledata.spillset.index()].slot.index()].alloc
        }
    }

    fn apply_allocations_and_insert_moves(&mut self) {
        log::debug!("blockparam_ins: {:?}", self.blockparam_ins);
        log::debug!("blockparam_outs: {:?}", self.blockparam_outs);

        /// We create "half-moves" in order to allow a single-scan
        /// strategy with a subsequent sort. Basically, the key idea
        /// is that as our single scan through a range for a vreg hits
        /// upon the source or destination of an edge-move, we emit a
        /// "half-move". These half-moves are carefully keyed in a
        /// particular sort order (the field order below is
        /// significant!) so that all half-moves on a given (from, to)
        /// block-edge appear contiguously, and then all moves from a
        /// given vreg appear contiguously. Within a given from-vreg,
        /// pick the first `Source` (there should only be one, but
        /// imprecision in liveranges due to loop handling sometimes
        /// means that a blockparam-out is also recognized as a normal-out),
        /// and then for each `Dest`, copy the source-alloc to that
        /// dest-alloc.
        #[derive(Clone, Debug, PartialEq, Eq)]
        struct HalfMove {
            key: u64,
            alloc: Allocation,
        }
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
        #[repr(u8)]
        enum HalfMoveKind {
            Source = 0,
            Dest = 1,
        }
        fn half_move_key(
            from_block: Block,
            to_block: Block,
            to_vreg: VRegIndex,
            kind: HalfMoveKind,
        ) -> u64 {
            assert!(from_block.index() < 1 << 21);
            assert!(to_block.index() < 1 << 21);
            assert!(to_vreg.index() < 1 << 21);
            ((from_block.index() as u64) << 43)
                | ((to_block.index() as u64) << 22)
                | ((to_vreg.index() as u64) << 1)
                | (kind as u8 as u64)
        }
        impl HalfMove {
            fn from_block(&self) -> Block {
                Block::new(((self.key >> 43) & ((1 << 21) - 1)) as usize)
            }
            fn to_block(&self) -> Block {
                Block::new(((self.key >> 22) & ((1 << 21) - 1)) as usize)
            }
            fn to_vreg(&self) -> VRegIndex {
                VRegIndex::new(((self.key >> 1) & ((1 << 21) - 1)) as usize)
            }
            fn kind(&self) -> HalfMoveKind {
                if self.key & 1 == 1 {
                    HalfMoveKind::Dest
                } else {
                    HalfMoveKind::Source
                }
            }
        }

        let mut half_moves: Vec<HalfMove> = vec![];

        let mut reuse_input_insts = vec![];

        let mut blockparam_in_idx = 0;
        let mut blockparam_out_idx = 0;
        for vreg in 0..self.vregs.len() {
            let vreg = VRegIndex::new(vreg);
            let defidx = self.vregs[vreg.index()].def;
            let defining_block = if defidx.is_valid() {
                self.cfginfo.insn_block[self.defs[defidx.index()].pos.inst.index()]
            } else if self.vregs[vreg.index()].blockparam.is_valid() {
                self.vregs[vreg.index()].blockparam
            } else {
                Block::invalid()
            };

            // For each range in each vreg, insert moves or
            // half-moves.  We also scan over `blockparam_ins` and
            // `blockparam_outs`, which are sorted by (block, vreg).
            let mut iter = self.vregs[vreg.index()].first_range;
            let mut prev = LiveRangeIndex::invalid();
            while iter.is_valid() {
                let alloc = self.get_alloc_for_range(iter);
                let range = self.ranges[iter.index()].range;
                log::debug!(
                    "apply_allocations: vreg {:?} LR {:?} with range {:?} has alloc {:?}",
                    vreg,
                    iter,
                    range,
                    alloc
                );
                debug_assert!(alloc != Allocation::none());

                if log::log_enabled!(log::Level::Debug) {
                    self.annotate(
                        range.from,
                        format!(
                            " <<< start v{} in {} (LR {})",
                            vreg.index(),
                            alloc,
                            iter.index()
                        ),
                    );
                    self.annotate(
                        range.to,
                        format!(
                            "     end   v{} in {} (LR {}) >>>",
                            vreg.index(),
                            alloc,
                            iter.index()
                        ),
                    );
                }

                // Does this range follow immediately after a prior
                // range in the same block? If so, insert a move (if
                // the allocs differ). We do this directly rather than
                // with half-moves because we eagerly know both sides
                // already (and also, half-moves are specific to
                // inter-block transfers).
                //
                // Note that we do *not* do this if there is also a
                // def exactly at `range.from`: it's possible that an
                // old liverange covers the Before pos of an inst, a
                // new liverange covers the After pos, and the def
                // also happens at After. In this case we don't want
                // to an insert a move after the instruction copying
                // the old liverange.
                //
                // Note also that we assert that the new range has to
                // start at the Before-point of an instruction; we
                // can't insert a move that logically happens just
                // before After (i.e. in the middle of a single
                // instruction).
                if prev.is_valid() {
                    let prev_alloc = self.get_alloc_for_range(prev);
                    let prev_range = self.ranges[prev.index()].range;
                    let def_idx = self.ranges[iter.index()].def;
                    let def_pos = if def_idx.is_valid() {
                        Some(self.defs[def_idx.index()].pos)
                    } else {
                        None
                    };
                    debug_assert!(prev_alloc != Allocation::none());
                    if prev_range.to == range.from
                        && !self.is_start_of_block(range.from)
                        && def_pos != Some(range.from)
                    {
                        log::debug!(
                            "prev LR {} abuts LR {} in same block; moving {} -> {} for v{}",
                            prev.index(),
                            iter.index(),
                            prev_alloc,
                            alloc,
                            vreg.index()
                        );
                        assert_eq!(range.from.pos, InstPosition::Before);
                        self.insert_move(range.from, InsertMovePrio::Regular, prev_alloc, alloc);
                    }
                }

                // Scan over blocks whose ends are covered by this
                // range. For each, for each successor that is not
                // already in this range (hence guaranteed to have the
                // same allocation) and if the vreg is live, add a
                // Source half-move.
                let mut block = self.cfginfo.insn_block[range.from.inst.index()];
                while block.is_valid() && block.index() < self.func.blocks() {
                    if range.to < self.cfginfo.block_exit[block.index()].next() {
                        break;
                    }
                    log::debug!("examining block with end in range: block{}", block.index());
                    for &succ in self.func.block_succs(block) {
                        log::debug!(
                            " -> has succ block {} with entry {:?}",
                            succ.index(),
                            self.cfginfo.block_entry[succ.index()]
                        );
                        if range.contains_point(self.cfginfo.block_entry[succ.index()]) {
                            continue;
                        }
                        log::debug!(" -> out of this range, requires half-move if live");
                        if self.liveins[succ.index()].get(vreg.index()) {
                            log::debug!("  -> live at input to succ, adding halfmove");
                            half_moves.push(HalfMove {
                                key: half_move_key(block, succ, vreg, HalfMoveKind::Source),
                                alloc,
                            });
                        }
                    }

                    // Scan forward in `blockparam_outs`, adding all
                    // half-moves for outgoing values to blockparams
                    // in succs.
                    log::debug!(
                        "scanning blockparam_outs for v{} block{}: blockparam_out_idx = {}",
                        vreg.index(),
                        block.index(),
                        blockparam_out_idx,
                    );
                    while blockparam_out_idx < self.blockparam_outs.len() {
                        let (from_vreg, from_block, to_block, to_vreg) =
                            self.blockparam_outs[blockparam_out_idx];
                        if (from_vreg, from_block) > (vreg, block) {
                            break;
                        }
                        if (from_vreg, from_block) == (vreg, block) {
                            log::debug!(
                                " -> found: from v{} block{} to v{} block{}",
                                from_vreg.index(),
                                from_block.index(),
                                to_vreg.index(),
                                to_vreg.index()
                            );
                            half_moves.push(HalfMove {
                                key: half_move_key(
                                    from_block,
                                    to_block,
                                    to_vreg,
                                    HalfMoveKind::Source,
                                ),
                                alloc,
                            });
                            if log::log_enabled!(log::Level::Debug) {
                                self.annotate(
                                    self.cfginfo.block_exit[block.index()],
                                    format!(
                                        "blockparam-out: block{} to block{}: v{} to v{} in {}",
                                        from_block.index(),
                                        to_block.index(),
                                        from_vreg.index(),
                                        to_vreg.index(),
                                        alloc
                                    ),
                                );
                            }
                        }
                        blockparam_out_idx += 1;
                    }

                    block = block.next();
                }

                // Scan over blocks whose beginnings are covered by
                // this range and for which the vreg is live at the
                // start of the block, and for which the def of the
                // vreg is not in this block. For each, for each
                // predecessor, add a Dest half-move.
                //
                // N.B.: why "def of this vreg is not in this block"?
                // Because live-range computation can over-approximate
                // (due to the way that we handle loops in a single
                // pass), especially if the program has irreducible
                // control flow and/or if blocks are not in RPO, it
                // may be the case that (i) the vreg is not *actually*
                // live into this block, but is *defined* in this
                // block. If the value is defined in this block,
                // because this is SSA, the value cannot be used
                // before the def and so we are not concerned about
                // any incoming allocation for it.
                let mut block = self.cfginfo.insn_block[range.from.inst.index()];
                if self.cfginfo.block_entry[block.index()] < range.from {
                    block = block.next();
                }
                while block.is_valid() && block.index() < self.func.blocks() {
                    if self.cfginfo.block_entry[block.index()] >= range.to {
                        break;
                    }

                    // Add half-moves for blockparam inputs.
                    log::debug!(
                        "scanning blockparam_ins at vreg {} block {}: blockparam_in_idx = {}",
                        vreg.index(),
                        block.index(),
                        blockparam_in_idx
                    );
                    while blockparam_in_idx < self.blockparam_ins.len() {
                        let (to_vreg, to_block, from_block) =
                            self.blockparam_ins[blockparam_in_idx];
                        if (to_vreg, to_block) > (vreg, block) {
                            break;
                        }
                        if (to_vreg, to_block) == (vreg, block) {
                            half_moves.push(HalfMove {
                                key: half_move_key(
                                    from_block,
                                    to_block,
                                    to_vreg,
                                    HalfMoveKind::Dest,
                                ),
                                alloc,
                            });
                            log::debug!(
                                "match: blockparam_in: v{} in block{} from block{} into {}",
                                to_vreg.index(),
                                to_block.index(),
                                from_block.index(),
                                alloc,
                            );
                            if log::log_enabled!(log::Level::Debug) {
                                self.annotate(
                                    self.cfginfo.block_entry[block.index()],
                                    format!(
                                        "blockparam-in: block{} to block{}:into v{} in {}",
                                        from_block.index(),
                                        to_block.index(),
                                        to_vreg.index(),
                                        alloc
                                    ),
                                );
                            }
                        }
                        blockparam_in_idx += 1;
                    }

                    // The below (range incoming into block) must be
                    // skipped if the def is in this block, as noted
                    // above.
                    if block == defining_block || !self.liveins[block.index()].get(vreg.index()) {
                        block = block.next();
                        continue;
                    }

                    log::debug!(
                        "scanning preds at vreg {} block {} for ends outside the range",
                        vreg.index(),
                        block.index()
                    );

                    // Now find any preds whose ends are not in the
                    // same range, and insert appropriate moves.
                    for &pred in self.func.block_preds(block) {
                        log::debug!(
                            "pred block {} has exit {:?}",
                            pred.index(),
                            self.cfginfo.block_exit[pred.index()]
                        );
                        if range.contains_point(self.cfginfo.block_exit[pred.index()]) {
                            continue;
                        }
                        log::debug!(" -> requires half-move");
                        half_moves.push(HalfMove {
                            key: half_move_key(pred, block, vreg, HalfMoveKind::Dest),
                            alloc,
                        });
                    }

                    block = block.next();
                }

                // If this is a blockparam vreg and the start of block
                // is in this range, add to blockparam_allocs.
                let (blockparam_block, blockparam_idx) =
                    self.cfginfo.vreg_def_blockparam[vreg.index()];
                if blockparam_block.is_valid()
                    && range.contains_point(self.cfginfo.block_entry[blockparam_block.index()])
                {
                    self.blockparam_allocs
                        .push((blockparam_block, blockparam_idx, vreg, alloc));
                }

                // Scan over def/uses and apply allocations.
                if self.ranges[iter.index()].def.is_valid() {
                    let defdata = &self.defs[self.ranges[iter.index()].def.index()];
                    debug_assert!(range.contains_point(defdata.pos));
                    let operand = defdata.operand;
                    let inst = defdata.pos.inst;
                    let slot = defdata.slot;
                    self.set_alloc(inst, slot, alloc);
                    if let OperandPolicy::Reuse(_) = operand.policy() {
                        reuse_input_insts.push(inst);
                    }
                }
                let mut use_iter = self.ranges[iter.index()].first_use;
                while use_iter.is_valid() {
                    let usedata = &self.uses[use_iter.index()];
                    debug_assert!(range.contains_point(usedata.pos));
                    let inst = usedata.pos.inst;
                    let slot = usedata.slot;
                    self.set_alloc(inst, slot, alloc);
                    use_iter = self.uses[use_iter.index()].next_use;
                }

                prev = iter;
                iter = self.ranges[iter.index()].next_in_reg;
            }
        }

        // Sort the half-moves list. For each (from, to,
        // from-vreg) tuple, find the from-alloc and all the
        // to-allocs, and insert moves on the block edge.
        half_moves.sort_by_key(|h| h.key);
        log::debug!("halfmoves: {:?}", half_moves);
        self.stats.halfmoves_count = half_moves.len();

        let mut i = 0;
        while i < half_moves.len() {
            // Find a Source.
            while i < half_moves.len() && half_moves[i].kind() != HalfMoveKind::Source {
                i += 1;
            }
            if i >= half_moves.len() {
                break;
            }
            let src = &half_moves[i];
            i += 1;

            // Find all Dests.
            let dest_key = src.key | 1;
            let first_dest = i;
            while i < half_moves.len() && half_moves[i].key == dest_key {
                i += 1;
            }
            let last_dest = i;

            log::debug!(
                "halfmove match: src {:?} dests {:?}",
                src,
                &half_moves[first_dest..last_dest]
            );

            // Determine the ProgPoint where moves on this (from, to)
            // edge should go:
            // - If there is more than one in-edge to `to`, then
            //   `from` must have only one out-edge; moves go at tail of
            //   `from` just before last Branch/Ret.
            // - Otherwise, there must be at most one in-edge to `to`,
            //   and moves go at start of `to`.
            let from_last_insn = self.func.block_insns(src.from_block()).last();
            let to_first_insn = self.func.block_insns(src.to_block()).first();
            let from_is_ret = self.func.is_ret(from_last_insn);
            let to_is_entry = self.func.entry_block() == src.to_block();
            let from_outs =
                self.func.block_succs(src.from_block()).len() + if from_is_ret { 1 } else { 0 };
            let to_ins =
                self.func.block_preds(src.to_block()).len() + if to_is_entry { 1 } else { 0 };

            let (insertion_point, prio) = if to_ins > 1 && from_outs <= 1 {
                (
                    // N.B.: "after" the branch should be interpreted
                    // by the user as happening before the actual
                    // branching action, but after the branch reads
                    // all necessary inputs. It's necessary to do this
                    // rather than to place the moves before the
                    // branch because the branch may have other
                    // actions than just the control-flow transfer,
                    // and these other actions may require other
                    // inputs (which should be read before the "edge"
                    // moves).
                    //
                    // Edits will only appear after the last (branch)
                    // instruction if the block has only a single
                    // successor; we do not expect the user to somehow
                    // duplicate or predicate these.
                    ProgPoint::after(from_last_insn),
                    InsertMovePrio::OutEdgeMoves,
                )
            } else if to_ins <= 1 {
                (
                    ProgPoint::before(to_first_insn),
                    InsertMovePrio::InEdgeMoves,
                )
            } else {
                panic!(
                    "Critical edge: can't insert moves between blocks {:?} and {:?}",
                    src.from_block(), src.to_block()
                );
            };

            let mut last = None;
            for dest in first_dest..last_dest {
                let dest = &half_moves[dest];
                debug_assert!(last != Some(dest.alloc));
                self.insert_move(insertion_point, prio, src.alloc, dest.alloc);
                last = Some(dest.alloc);
            }
        }

        // Handle multi-fixed-reg constraints by copying.
        for (progpoint, from_preg, to_preg) in
            std::mem::replace(&mut self.multi_fixed_reg_fixups, vec![])
        {
            log::debug!(
                "multi-fixed-move constraint at {:?} from p{} to p{}",
                progpoint,
                from_preg.index(),
                to_preg.index()
            );
            self.insert_move(
                progpoint,
                InsertMovePrio::MultiFixedReg,
                Allocation::reg(self.pregs[from_preg.index()].reg),
                Allocation::reg(self.pregs[to_preg.index()].reg),
            );
        }

        // Handle outputs that reuse inputs: copy beforehand, then set
        // input's alloc to output's.
        //
        // Note that the output's allocation may not *actually* be
        // valid until InstPosition::After, but the reused input may
        // occur at InstPosition::Before. This may appear incorrect,
        // but we make it work by ensuring that all *other* inputs are
        // extended to InstPosition::After so that the def will not
        // interfere. (The liveness computation code does this -- we
        // do not require the user to do so.)
        //
        // One might ask: why not insist that input-reusing defs occur
        // at InstPosition::Before? this would be correct, but would
        // mean that the reused input and the reusing output
        // interfere, *guaranteeing* that every such case would
        // require a move. This is really bad on ISAs (like x86) where
        // reused inputs are ubiquitous.
        //
        // Another approach might be to put the def at Before, and
        // trim the reused input's liverange back to the previous
        // instruction's After. This is kind of OK until (i) a block
        // boundary occurs between the prior inst and this one, or
        // (ii) any moves/spills/reloads occur between the two
        // instructions. We really do need the input to be live at
        // this inst's Before.
        //
        // In principle what we really need is a "BeforeBefore"
        // program point, but we don't want to introduce that
        // everywhere and pay the cost of twice as many ProgPoints
        // throughout the allocator.
        //
        // Or we could introduce a separate move instruction -- this
        // is the approach that regalloc.rs takes with "mod" operands
        // -- but that is also costly.
        //
        // So we take this approach (invented by IonMonkey -- somewhat
        // hard to discern, though see [0] for a comment that makes
        // this slightly less unclear) to avoid interference between
        // the actual reused input and reusing output, ensure
        // interference (hence no incorrectness) between other inputs
        // and the reusing output, and not require a separate explicit
        // move instruction.
        //
        // [0] https://searchfox.org/mozilla-central/rev/3a798ef9252896fb389679f06dd3203169565af0/js/src/jit/shared/Lowering-shared-inl.h#108-110
        for inst in reuse_input_insts {
            let mut input_reused: SmallVec<[usize; 4]> = smallvec![];
            for output_idx in 0..self.func.inst_operands(inst).len() {
                let operand = self.func.inst_operands(inst)[output_idx];
                if let OperandPolicy::Reuse(input_idx) = operand.policy() {
                    debug_assert!(!input_reused.contains(&input_idx));
                    debug_assert_eq!(operand.pos(), OperandPos::After);
                    input_reused.push(input_idx);
                    let input_alloc = self.get_alloc(inst, input_idx);
                    let output_alloc = self.get_alloc(inst, output_idx);
                    log::debug!(
                        "reuse-input inst {:?}: output {} has alloc {:?}, input {} has alloc {:?}",
                        inst,
                        output_idx,
                        output_alloc,
                        input_idx,
                        input_alloc
                    );
                    if input_alloc != output_alloc {
                        if log::log_enabled!(log::Level::Debug) {
                            self.annotate(
                                ProgPoint::before(inst),
                                format!(" reuse-input-copy: {} -> {}", input_alloc, output_alloc),
                            );
                        }
                        self.insert_move(
                            ProgPoint::before(inst),
                            InsertMovePrio::ReusedInput,
                            input_alloc,
                            output_alloc,
                        );
                        self.set_alloc(inst, input_idx, output_alloc);
                    }
                }
            }
        }
    }

    fn resolve_inserted_moves(&mut self) {
        // For each program point, gather all moves together. Then
        // resolve (see cases below).
        let mut i = 0;
        self.inserted_moves
            .sort_by_key(|m| (m.pos.to_index(), m.prio));
        while i < self.inserted_moves.len() {
            let start = i;
            let pos = self.inserted_moves[i].pos;
            let prio = self.inserted_moves[i].prio;
            while i < self.inserted_moves.len()
                && self.inserted_moves[i].pos == pos
                && self.inserted_moves[i].prio == prio
            {
                i += 1;
            }
            let moves = &self.inserted_moves[start..i];

            // Get the regclass from one of the moves.
            let regclass = moves[0].from_alloc.class();

            // All moves in `moves` semantically happen in
            // parallel. Let's resolve these to a sequence of moves
            // that can be done one at a time.
            let mut parallel_moves = ParallelMoves::new(Allocation::reg(
                self.env.scratch_by_class[regclass as u8 as usize],
            ));
            log::debug!("parallel moves at pos {:?} prio {:?}", pos, prio);
            for m in moves {
                if m.from_alloc != m.to_alloc {
                    log::debug!(" {} -> {}", m.from_alloc, m.to_alloc,);
                    parallel_moves.add(m.from_alloc, m.to_alloc);
                }
            }

            let resolved = parallel_moves.resolve();

            for (src, dst) in resolved {
                log::debug!("  resolved: {} -> {}", src, dst);
                self.add_edit(pos, prio, Edit::Move { from: src, to: dst });
            }
        }

        // Add edits to describe blockparam locations too. This is
        // required by the checker. This comes after any edge-moves.
        self.blockparam_allocs
            .sort_by_key(|&(block, idx, _, _)| (block, idx));
        self.stats.blockparam_allocs_count = self.blockparam_allocs.len();
        let mut i = 0;
        while i < self.blockparam_allocs.len() {
            let start = i;
            let block = self.blockparam_allocs[i].0;
            while i < self.blockparam_allocs.len() && self.blockparam_allocs[i].0 == block {
                i += 1;
            }
            let params = &self.blockparam_allocs[start..i];
            let vregs = params
                .iter()
                .map(|(_, _, vreg_idx, _)| self.vregs[vreg_idx.index()].reg)
                .collect::<Vec<_>>();
            let allocs = params
                .iter()
                .map(|(_, _, _, alloc)| *alloc)
                .collect::<Vec<_>>();
            assert_eq!(vregs.len(), self.func.block_params(block).len());
            assert_eq!(allocs.len(), self.func.block_params(block).len());
            self.add_edit(
                self.cfginfo.block_entry[block.index()],
                InsertMovePrio::BlockParam,
                Edit::BlockParams { vregs, allocs },
            );
        }

        // Ensure edits are in sorted ProgPoint order.
        self.edits.sort_by_key(|&(pos, prio, _)| (pos, prio));
        self.stats.edits_count = self.edits.len();

        // Add debug annotations.
        if log::log_enabled!(log::Level::Debug) {
            for i in 0..self.edits.len() {
                let &(pos, _, ref edit) = &self.edits[i];
                match edit {
                    &Edit::Move { from, to } => {
                        self.annotate(
                            ProgPoint::from_index(pos),
                            format!("move {} -> {}", from, to),
                        );
                    }
                    &Edit::BlockParams {
                        ref vregs,
                        ref allocs,
                    } => {
                        let s = format!("blockparams vregs:{:?} allocs:{:?}", vregs, allocs);
                        self.annotate(ProgPoint::from_index(pos), s);
                    }
                }
            }
        }
    }

    fn add_edit(&mut self, pos: ProgPoint, prio: InsertMovePrio, edit: Edit) {
        match &edit {
            &Edit::Move { from, to } if from == to => return,
            _ => {}
        }

        self.edits.push((pos.to_index(), prio, edit));
    }

    fn compute_stackmaps(&mut self) {}

    pub(crate) fn init(&mut self) -> Result<(), RegAllocError> {
        self.create_pregs_and_vregs();
        self.compute_liveness();
        self.compute_hot_code();
        self.merge_vreg_bundles();
        self.queue_bundles();
        if log::log_enabled!(log::Level::Debug) {
            self.dump_state();
        }
        Ok(())
    }

    pub(crate) fn run(&mut self) -> Result<(), RegAllocError> {
        self.process_bundles();
        self.try_allocating_regs_for_spilled_bundles();
        self.allocate_spillslots();
        self.apply_allocations_and_insert_moves();
        self.resolve_inserted_moves();
        self.compute_stackmaps();
        Ok(())
    }

    fn annotate(&mut self, progpoint: ProgPoint, s: String) {
        if log::log_enabled!(log::Level::Debug) {
            self.debug_annotations
                .entry(progpoint)
                .or_insert_with(|| vec![])
                .push(s);
        }
    }

    fn dump_results(&self) {
        log::debug!("=== REGALLOC RESULTS ===");
        for block in 0..self.func.blocks() {
            let block = Block::new(block);
            log::debug!(
                "block{}: [succs {:?} preds {:?}]",
                block.index(),
                self.func
                    .block_succs(block)
                    .iter()
                    .map(|b| b.index())
                    .collect::<Vec<_>>(),
                self.func
                    .block_preds(block)
                    .iter()
                    .map(|b| b.index())
                    .collect::<Vec<_>>()
            );
            for inst in self.func.block_insns(block).iter() {
                for annotation in self
                    .debug_annotations
                    .get(&ProgPoint::before(inst))
                    .map(|v| &v[..])
                    .unwrap_or(&[])
                {
                    log::debug!("  inst{}-pre: {}", inst.index(), annotation);
                }
                let ops = self
                    .func
                    .inst_operands(inst)
                    .iter()
                    .map(|op| format!("{}", op))
                    .collect::<Vec<_>>();
                let clobbers = self
                    .func
                    .inst_clobbers(inst)
                    .iter()
                    .map(|preg| format!("{}", preg))
                    .collect::<Vec<_>>();
                let allocs = (0..ops.len())
                    .map(|i| format!("{}", self.get_alloc(inst, i)))
                    .collect::<Vec<_>>();
                let opname = if self.func.is_branch(inst) {
                    "br"
                } else if self.func.is_call(inst) {
                    "call"
                } else if self.func.is_ret(inst) {
                    "ret"
                } else {
                    "op"
                };
                let args = ops
                    .iter()
                    .zip(allocs.iter())
                    .map(|(op, alloc)| format!("{} [{}]", op, alloc))
                    .collect::<Vec<_>>();
                let clobbers = if clobbers.is_empty() {
                    "".to_string()
                } else {
                    format!(" [clobber: {}]", clobbers.join(", "))
                };
                log::debug!(
                    "  inst{}: {} {}{}",
                    inst.index(),
                    opname,
                    args.join(", "),
                    clobbers
                );
                for annotation in self
                    .debug_annotations
                    .get(&ProgPoint::after(inst))
                    .map(|v| &v[..])
                    .unwrap_or(&[])
                {
                    log::debug!("  inst{}-post: {}", inst.index(), annotation);
                }
            }
        }
    }
}

pub fn run<F: Function>(func: &F, mach_env: &MachineEnv) -> Result<Output, RegAllocError> {
    let cfginfo = CFGInfo::new(func);
    validate_ssa(func, &cfginfo)?;

    let mut env = Env::new(func, mach_env, cfginfo);
    env.init()?;

    env.run()?;

    if log::log_enabled!(log::Level::Debug) {
        env.dump_results();
    }

    Ok(Output {
        edits: env
            .edits
            .into_iter()
            .map(|(pos, _, edit)| (ProgPoint::from_index(pos), edit))
            .collect(),
        allocs: env.allocs,
        inst_alloc_offsets: env.inst_alloc_offsets,
        num_spillslots: env.num_spillslots as usize,
        stats: env.stats,
    })
}
