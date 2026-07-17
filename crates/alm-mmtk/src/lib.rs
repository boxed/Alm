//! alm's MMTk binding — the native GC alternative to Boehm.
//!
//! Compiled with a modern rustc (MMTk needs recent Rust) into a staticlib with
//! a C ABI, and linked into native alm programs alongside — eventually instead
//! of — libgc. The 1.72.1-pinned runtime (`native_runtime.rs`) calls the C ABI.
//!
//! Milestone 1 (this file): the `NoGC` plan — a bump allocator that never
//! collects — to validate the build/link/C-ABI architecture and MMTk's
//! allocation throughput against the Boehm path. Milestone 2 swaps in Immix
//! with a conservative object model (`is_mmtk_object`/VO-bit) + conservative
//! stack roots, which is what makes it collect and thus shippable.
#![allow(static_mut_refs)]

use std::ops::Range;

use mmtk::util::alloc::{AllocationError, BumpAllocator, BumpPointer};
use mmtk::util::copy::{CopySemantics, GCWorkerCopyContext};
use mmtk::util::options::{GCTriggerSelector, PlanSelector};
use mmtk::util::{Address, ObjectReference, VMMutatorThread, VMThread, VMWorkerThread};
use mmtk::vm::{
    ActivePlan, Collection, GCThreadContext, ObjectModel, ObjectTracer, ObjectTracerContext,
    ReferenceGlue, RootsWorkFactory, Scanning, SlotVisitor, VMBinding, VMGlobalLogBitSpec,
    VMLocalForwardingBitsSpec, VMLocalForwardingPointerSpec, VMLocalLOSMarkNurserySpec,
    VMLocalMarkBitSpec,
};
use mmtk::{memory_manager, AllocationSemantics, MMTKBuilder, Mutator, ObjectQueue, MMTK};

#[derive(Default)]
pub struct AlmVM;

impl VMBinding for AlmVM {
    type VMSlot = Address;
    type VMMemorySlice = Range<Address>;
    type VMActivePlan = AlmVM;
    type VMCollection = AlmVM;
    type VMObjectModel = AlmVM;
    type VMReferenceGlue = AlmVM;
    type VMScanning = AlmVM;
    const MAX_ALIGNMENT: usize = 1 << 6;
}

impl ObjectModel<AlmVM> for AlmVM {
    const GLOBAL_LOG_BIT_SPEC: VMGlobalLogBitSpec = VMGlobalLogBitSpec::in_header(0);
    const LOCAL_FORWARDING_POINTER_SPEC: VMLocalForwardingPointerSpec =
        VMLocalForwardingPointerSpec::in_header(0);
    const LOCAL_FORWARDING_BITS_SPEC: VMLocalForwardingBitsSpec =
        VMLocalForwardingBitsSpec::in_header(0);
    const LOCAL_MARK_BIT_SPEC: VMLocalMarkBitSpec = VMLocalMarkBitSpec::in_header(0);
    const LOCAL_LOS_MARK_NURSERY_SPEC: VMLocalLOSMarkNurserySpec =
        VMLocalLOSMarkNurserySpec::in_header(0);
    #[cfg(feature = "object_pinning")]
    const LOCAL_PINNING_BIT_SPEC: mmtk::vm::VMLocalPinningBitSpec =
        mmtk::vm::VMLocalPinningBitSpec::in_header(0);
    // Our objects have no MMTk header: the object reference IS the allocation
    // start (offset 0). Milestone 2 revisits this for the conservative model.
    const OBJECT_REF_OFFSET_LOWER_BOUND: isize = 0;

    // NoGC never marks, copies, or scans, so the collection-only hooks are
    // unreachable until Milestone 2 wires up Immix.
    fn copy(_: ObjectReference, _: CopySemantics, _: &mut GCWorkerCopyContext<AlmVM>) -> ObjectReference {
        unreachable!("NoGC never copies")
    }
    fn copy_to(_: ObjectReference, _: ObjectReference, _: Address) -> Address {
        unreachable!("NoGC never copies")
    }
    fn get_current_size(_: ObjectReference) -> usize {
        unreachable!("NoGC never queries size")
    }
    fn get_size_when_copied(_: ObjectReference) -> usize {
        unreachable!()
    }
    fn get_align_when_copied(_: ObjectReference) -> usize {
        unreachable!()
    }
    fn get_align_offset_when_copied(_: ObjectReference) -> usize {
        unreachable!()
    }
    fn get_type_descriptor(_: ObjectReference) -> &'static [i8] {
        unreachable!()
    }
    fn get_reference_when_copied_to(_: ObjectReference, _: Address) -> ObjectReference {
        unreachable!()
    }
    fn ref_to_object_start(object: ObjectReference) -> Address {
        object.to_raw_address()
    }
    fn ref_to_header(object: ObjectReference) -> Address {
        object.to_raw_address()
    }
    fn dump_object(_: ObjectReference) {
        unreachable!()
    }
}

impl ActivePlan<AlmVM> for AlmVM {
    fn number_of_mutators() -> usize {
        1
    }
    fn is_mutator(_tls: VMThread) -> bool {
        true
    }
    fn mutator(_tls: VMMutatorThread) -> &'static mut Mutator<AlmVM> {
        unsafe { MUTATOR.as_mut().expect("mutator not bound") }
    }
    fn mutators<'a>() -> Box<dyn Iterator<Item = &'a mut Mutator<AlmVM>> + 'a> {
        unreachable!("NoGC never iterates mutators")
    }
    fn vm_trace_object<Q: ObjectQueue>(
        _queue: &mut Q,
        _object: ObjectReference,
        _worker: &mut mmtk::scheduler::GCWorker<AlmVM>,
    ) -> ObjectReference {
        unreachable!()
    }
}

impl Collection<AlmVM> for AlmVM {
    fn stop_all_mutators<F>(_tls: VMWorkerThread, _visitor: F)
    where
        F: FnMut(&'static mut Mutator<AlmVM>),
    {
        unreachable!("NoGC never stops mutators")
    }
    fn resume_mutators(_tls: VMWorkerThread) {
        unreachable!()
    }
    fn block_for_gc(_tls: VMMutatorThread) {
        unreachable!("NoGC never blocks for GC")
    }
    fn spawn_gc_thread(_tls: VMThread, _ctx: GCThreadContext<AlmVM>) {
        // NoGC spawns no collector threads.
    }
    fn out_of_memory(_tls: VMThread, _err: AllocationError) {
        eprintln!("alm-mmtk: out of memory");
        std::process::abort();
    }
    fn schedule_finalization(_tls: VMWorkerThread) {}
    fn post_forwarding(_tls: VMWorkerThread) {}
    fn is_collection_enabled() -> bool {
        false
    }
    fn vm_live_bytes() -> usize {
        0
    }
}

impl Scanning<AlmVM> for AlmVM {
    fn support_slot_enqueuing(_tls: VMWorkerThread, _object: ObjectReference) -> bool {
        false
    }
    fn scan_object<SV: SlotVisitor<Address>>(_tls: VMWorkerThread, _object: ObjectReference, _sv: &mut SV) {
        unreachable!("NoGC never scans")
    }
    fn scan_object_and_trace_edges<OT: ObjectTracer>(
        _tls: VMWorkerThread,
        _object: ObjectReference,
        _ot: &mut OT,
    ) {
        unreachable!()
    }
    fn scan_roots_in_mutator_thread(
        _tls: VMWorkerThread,
        _mutator: &'static mut Mutator<AlmVM>,
        _factory: impl RootsWorkFactory<Address>,
    ) {
        unreachable!()
    }
    fn scan_vm_specific_roots(_tls: VMWorkerThread, _factory: impl RootsWorkFactory<Address>) {
        unreachable!()
    }
    fn notify_initial_thread_scan_complete(_partial: bool, _tls: VMWorkerThread) {}
    fn supports_return_barrier() -> bool {
        false
    }
    fn prepare_for_roots_re_scanning() {}
    fn process_weak_refs(
        _worker: &mut mmtk::scheduler::GCWorker<AlmVM>,
        _tracer: impl ObjectTracerContext<AlmVM>,
    ) -> bool {
        false
    }
}

impl ReferenceGlue<AlmVM> for AlmVM {
    type FinalizableType = ObjectReference;
    fn clear_referent(_new: ObjectReference) {}
    fn set_referent(_reff: ObjectReference, _referent: ObjectReference) {}
    fn get_referent(_object: ObjectReference) -> Option<ObjectReference> {
        None
    }
    fn enqueue_references(_references: &[ObjectReference], _tls: VMWorkerThread) {}
}

// ---- C ABI ----
//
// Single-threaded: the native benchmark runs the program on one mutator.
// (Multi-threaded programs come with the Milestone-2 collector work.)

static mut MMTK_INSTANCE: Option<&'static MMTK<AlmVM>> = None;
static mut MUTATOR: Option<Box<Mutator<AlmVM>>> = None;
/// Address of the default allocator's `BumpPointer` inside the (heap-pinned)
/// mutator, so the runtime can inline the bump fast path instead of paying a
/// C-ABI call per allocation. Stable for the mutator's lifetime.
static mut BUMP_PTR: *mut BumpPointer = std::ptr::null_mut();

/// Initialize MMTk with the NoGC plan and a fixed heap, and bind the main
/// mutator. `heap_bytes` caps the reservation.
#[no_mangle]
pub extern "C" fn almmtk_init(heap_bytes: usize) {
    let mut builder = MMTKBuilder::new();
    builder.options.plan.set(PlanSelector::NoGC);
    builder
        .options
        .gc_trigger
        .set(GCTriggerSelector::FixedHeapSize(heap_bytes));
    let mmtk: &'static MMTK<AlmVM> = Box::leak(memory_manager::mmtk_init::<AlmVM>(&builder));
    let mut mutator = memory_manager::bind_mutator(mmtk, VMMutatorThread(VMThread::UNINITIALIZED));
    // Cache the address of the default allocator's bump pointer (NoGC's Default
    // semantics maps to a BumpAllocator) for the runtime's inline fast path.
    let bp: *mut BumpPointer = unsafe {
        let bump = mutator
            .allocator_impl_mut_for_semantic::<BumpAllocator<AlmVM>>(AllocationSemantics::Default);
        &mut bump.bump_pointer
    };
    unsafe {
        MMTK_INSTANCE = Some(mmtk);
        MUTATOR = Some(mutator);
        BUMP_PTR = bp;
    }
}

/// Address of the mutator's bump pointer (`#[repr(C)] { cursor, limit }` — two
/// words). The runtime bumps `cursor` inline while `cursor + size <= limit`,
/// calling `almmtk_alloc` only on the slow path.
#[no_mangle]
pub extern "C" fn almmtk_bump_pointer() -> *mut BumpPointer {
    unsafe { BUMP_PTR }
}

/// Bump-allocate `size` bytes with `align`. Returns the raw start.
#[no_mangle]
pub extern "C" fn almmtk_alloc(size: usize, align: usize) -> *mut u8 {
    let mutator = unsafe { MUTATOR.as_mut().expect("almmtk_init not called") };
    let addr = memory_manager::alloc::<AlmVM>(mutator, size, align, 0, AllocationSemantics::Default);
    addr.to_mut_ptr::<u8>()
}
