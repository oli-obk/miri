use rustc::middle::const_val;
use rustc::hir::def_id::DefId;
use rustc::mir::mir_map::MirMap;
use rustc::mir::repr as mir;
use rustc::traits::Reveal;
use rustc::ty::layout::{self, Layout, Size};
use rustc::ty::subst::{self, Subst, Substs};
use rustc::ty::{self, Ty, TyCtxt, TypeFoldable};
use rustc::util::nodemap::DefIdMap;
use rustc_data_structures::indexed_vec::Idx;
use std::cell::RefCell;
use std::ops::Deref;
use std::rc::Rc;
use std::iter;
use syntax::codemap::{self, DUMMY_SP};

use error::{EvalError, EvalResult};
use memory::{Memory, Pointer, AllocId};
use primval::{self, PrimVal};

use std::collections::HashMap;

mod step;
mod terminator;
mod cast;
mod vtable;

pub struct EvalContext<'a, 'tcx: 'a> {
    /// The results of the type checker, from rustc.
    tcx: TyCtxt<'a, 'tcx, 'tcx>,

    /// A mapping from NodeIds to Mir, from rustc. Only contains MIR for crate-local items.
    mir_map: &'a MirMap<'tcx>,

    /// A local cache from DefIds to Mir for non-crate-local items.
    mir_cache: RefCell<DefIdMap<Rc<mir::Mir<'tcx>>>>,

    /// The virtual memory system.
    memory: Memory<'a, 'tcx>,

    /// Precomputed statics, constants and promoteds.
    statics: HashMap<ConstantId<'tcx>, Pointer>,

    /// The virtual call stack.
    stack: Vec<Frame<'a, 'tcx>>,

    /// The maximum number of stack frames allowed
    stack_limit: usize,
}

/// A stack frame.
pub struct Frame<'a, 'tcx: 'a> {
    ////////////////////////////////////////////////////////////////////////////////
    // Function and callsite information
    ////////////////////////////////////////////////////////////////////////////////

    /// The MIR for the function called on this frame.
    pub mir: CachedMir<'a, 'tcx>,

    /// The def_id of the current function.
    pub def_id: DefId,

    /// type substitutions for the current function invocation.
    pub substs: &'tcx Substs<'tcx>,

    /// The span of the call site.
    pub span: codemap::Span,

    ////////////////////////////////////////////////////////////////////////////////
    // Return pointer and local allocations
    ////////////////////////////////////////////////////////////////////////////////

    /// A pointer for writing the return value of the current call if it's not a diverging call.
    pub return_ptr: Option<Pointer>,

    /// The block to return to when returning from the current stack frame
    pub return_to_block: StackPopCleanup,

    /// The list of locals for the current function, stored in order as
    /// `[arguments..., variables..., temporaries...]`. The variables begin at `self.var_offset`
    /// and the temporaries at `self.temp_offset`.
    pub locals: Vec<Pointer>,

    /// The offset of the first variable in `self.locals`.
    pub var_offset: usize,

    /// The offset of the first temporary in `self.locals`.
    pub temp_offset: usize,

    ////////////////////////////////////////////////////////////////////////////////
    // Current position within the function
    ////////////////////////////////////////////////////////////////////////////////

    /// The block that is currently executed (or will be executed after the above call stacks
    /// return).
    pub block: mir::BasicBlock,

    /// The index of the currently evaluated statment.
    pub stmt: usize,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Lvalue {
    ptr: Pointer,
    extra: LvalueExtra,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum LvalueExtra {
    None,
    Length(u64),
    Vtable(Pointer),
    DowncastVariant(usize),
}

#[derive(Clone)]
pub enum CachedMir<'mir, 'tcx: 'mir> {
    Ref(&'mir mir::Mir<'tcx>),
    Owned(Rc<mir::Mir<'tcx>>)
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
/// Uniquely identifies a specific constant or static
struct ConstantId<'tcx> {
    /// the def id of the constant/static or in case of promoteds, the def id of the function they belong to
    def_id: DefId,
    /// In case of statics and constants this is `Substs::empty()`, so only promoteds and associated
    /// constants actually have something useful here. We could special case statics and constants,
    /// but that would only require more branching when working with constants, and not bring any
    /// real benefits.
    substs: &'tcx Substs<'tcx>,
    kind: ConstantKind,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
enum ConstantKind {
    Promoted(mir::Promoted),
    /// Statics, constants and associated constants
    Global,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum StackPopCleanup {
    /// The stackframe existed to compute the initial value of a static/constant, make sure the
    /// static isn't modifyable afterwards
    Freeze(AllocId),
    /// A regular stackframe added due to a function call will need to get forwarded to the next
    /// block
    Goto(mir::BasicBlock),
    /// The main function and diverging functions have nowhere to return to
    None,
}

impl<'a, 'tcx> EvalContext<'a, 'tcx> {
    pub fn new(tcx: TyCtxt<'a, 'tcx, 'tcx>, mir_map: &'a MirMap<'tcx>, memory_size: usize, stack_limit: usize) -> Self {
        EvalContext {
            tcx: tcx,
            mir_map: mir_map,
            mir_cache: RefCell::new(DefIdMap()),
            memory: Memory::new(&tcx.data_layout, memory_size),
            statics: HashMap::new(),
            stack: Vec::new(),
            stack_limit: stack_limit,
        }
    }

    pub fn alloc_ret_ptr(&mut self, ty: Ty<'tcx>, substs: &'tcx Substs<'tcx>) -> EvalResult<'tcx, Pointer> {
        let size = self.type_size_with_substs(ty, substs);
        let align = self.type_align_with_substs(ty, substs);
        self.memory.allocate(size, align)
    }

    pub fn memory(&self) -> &Memory<'a, 'tcx> {
        &self.memory
    }

    pub fn memory_mut(&mut self) -> &mut Memory<'a, 'tcx> {
        &mut self.memory
    }

    pub fn stack(&self) -> &[Frame<'a, 'tcx>] {
        &self.stack
    }

    // TODO(solson): Try making const_to_primval instead.
    fn const_to_ptr(&mut self, const_val: &const_val::ConstVal) -> EvalResult<'tcx, Pointer> {
        use rustc::middle::const_val::ConstVal::*;
        use rustc_const_math::{ConstInt, ConstIsize, ConstUsize, ConstFloat};
        macro_rules! i2p {
            ($i:ident, $n:expr) => {{
                let ptr = self.memory.allocate($n, $n)?;
                self.memory.write_int(ptr, $i as i64, $n)?;
                Ok(ptr)
            }}
        }
        match *const_val {
            Float(ConstFloat::F32(f)) => {
                let ptr = self.memory.allocate(4, 4)?;
                self.memory.write_f32(ptr, f)?;
                Ok(ptr)
            },
            Float(ConstFloat::F64(f)) => {
                let ptr = self.memory.allocate(8, 8)?;
                self.memory.write_f64(ptr, f)?;
                Ok(ptr)
            },
            Float(ConstFloat::FInfer{..}) |
            Integral(ConstInt::Infer(_)) |
            Integral(ConstInt::InferSigned(_)) => bug!("uninferred constants only exist before typeck"),
            Integral(ConstInt::I8(i)) => i2p!(i, 1),
            Integral(ConstInt::U8(i)) => i2p!(i, 1),
            Integral(ConstInt::Isize(ConstIsize::Is16(i))) |
            Integral(ConstInt::I16(i)) => i2p!(i, 2),
            Integral(ConstInt::Usize(ConstUsize::Us16(i))) |
            Integral(ConstInt::U16(i)) => i2p!(i, 2),
            Integral(ConstInt::Isize(ConstIsize::Is32(i))) |
            Integral(ConstInt::I32(i)) => i2p!(i, 4),
            Integral(ConstInt::Usize(ConstUsize::Us32(i))) |
            Integral(ConstInt::U32(i)) => i2p!(i, 4),
            Integral(ConstInt::Isize(ConstIsize::Is64(i))) |
            Integral(ConstInt::I64(i)) => i2p!(i, 8),
            Integral(ConstInt::Usize(ConstUsize::Us64(i))) |
            Integral(ConstInt::U64(i)) => i2p!(i, 8),
            Str(ref s) => {
                let psize = self.memory.pointer_size();
                let static_ptr = self.memory.allocate(s.len(), 1)?;
                let ptr = self.memory.allocate(psize * 2, psize)?;
                let (ptr, extra) = self.get_fat_ptr(ptr);
                self.memory.write_bytes(static_ptr, s.as_bytes())?;
                self.memory.write_ptr(ptr, static_ptr)?;
                self.memory.write_usize(extra, s.len() as u64)?;
                Ok(ptr)
            }
            ByteStr(ref bs) => {
                let psize = self.memory.pointer_size();
                let static_ptr = self.memory.allocate(bs.len(), 1)?;
                let ptr = self.memory.allocate(psize, psize)?;
                self.memory.write_bytes(static_ptr, bs)?;
                self.memory.write_ptr(ptr, static_ptr)?;
                Ok(ptr)
            }
            Bool(b) => {
                let ptr = self.memory.allocate(1, 1)?;
                self.memory.write_bool(ptr, b)?;
                Ok(ptr)
            }
            Char(c) => {
                let ptr = self.memory.allocate(4, 4)?;
                self.memory.write_uint(ptr, c as u64, 4)?;
                Ok(ptr)
            },
            Struct(_node_id)  => unimplemented!(),
            Tuple(_node_id)   => unimplemented!(),
            Function(_def_id) => unimplemented!(),
            Array(_, _)       => unimplemented!(),
            Repeat(_, _)      => unimplemented!(),
            Dummy             => unimplemented!(),
        }
    }

    fn type_is_sized(&self, ty: Ty<'tcx>) -> bool {
        // generics are weird, don't run this function on a generic
        assert!(!ty.needs_subst());
        ty.is_sized(self.tcx, &self.tcx.empty_parameter_environment(), DUMMY_SP)
    }

    pub fn load_mir(&self, def_id: DefId) -> CachedMir<'a, 'tcx> {
        if def_id.is_local() {
            CachedMir::Ref(self.mir_map.map.get(&def_id).unwrap())
        } else {
            let mut mir_cache = self.mir_cache.borrow_mut();
            if let Some(mir) = mir_cache.get(&def_id) {
                return CachedMir::Owned(mir.clone());
            }

            let cs = &self.tcx.sess.cstore;
            let mir = cs.maybe_get_item_mir(self.tcx, def_id).unwrap_or_else(|| {
                panic!("no mir for `{}`", self.tcx.item_path_str(def_id));
            });
            let cached = Rc::new(mir);
            mir_cache.insert(def_id, cached.clone());
            CachedMir::Owned(cached)
        }
    }

    pub fn monomorphize(&self, ty: Ty<'tcx>, substs: &'tcx Substs<'tcx>) -> Ty<'tcx> {
        let substituted = ty.subst(self.tcx, substs);
        self.tcx.normalize_associated_type(&substituted)
    }

    fn type_size(&self, ty: Ty<'tcx>) -> usize {
        self.type_size_with_substs(ty, self.substs())
    }

    fn type_align(&self, ty: Ty<'tcx>) -> usize {
        self.type_align_with_substs(ty, self.substs())
    }

    fn type_size_with_substs(&self, ty: Ty<'tcx>, substs: &'tcx Substs<'tcx>) -> usize {
        self.type_layout_with_substs(ty, substs).size(&self.tcx.data_layout).bytes() as usize
    }

    fn type_align_with_substs(&self, ty: Ty<'tcx>, substs: &'tcx Substs<'tcx>) -> usize {
        self.type_layout_with_substs(ty, substs).align(&self.tcx.data_layout).abi() as usize
    }

    fn type_layout(&self, ty: Ty<'tcx>) -> &'tcx Layout {
        self.type_layout_with_substs(ty, self.substs())
    }

    fn type_layout_with_substs(&self, ty: Ty<'tcx>, substs: &'tcx Substs<'tcx>) -> &'tcx Layout {
        // TODO(solson): Is this inefficient? Needs investigation.
        let ty = self.monomorphize(ty, substs);

        self.tcx.infer_ctxt(None, None, Reveal::All).enter(|infcx| {
            // TODO(solson): Report this error properly.
            ty.layout(&infcx).unwrap()
        })
    }

    pub fn push_stack_frame(
        &mut self,
        def_id: DefId,
        span: codemap::Span,
        mir: CachedMir<'a, 'tcx>,
        substs: &'tcx Substs<'tcx>,
        return_ptr: Option<Pointer>,
        return_to_block: StackPopCleanup,
    ) -> EvalResult<'tcx, ()> {
        let arg_tys = mir.arg_decls.iter().map(|a| a.ty);
        let var_tys = mir.var_decls.iter().map(|v| v.ty);
        let temp_tys = mir.temp_decls.iter().map(|t| t.ty);

        let num_args = mir.arg_decls.len();
        let num_vars = mir.var_decls.len();

        ::log_settings::settings().indentation += 1;

        let locals: EvalResult<'tcx, Vec<Pointer>> = arg_tys.chain(var_tys).chain(temp_tys).map(|ty| {
            let size = self.type_size_with_substs(ty, substs);
            let align = self.type_align_with_substs(ty, substs);
            self.memory.allocate(size, align)
        }).collect();

        self.stack.push(Frame {
            mir: mir.clone(),
            block: mir::START_BLOCK,
            return_ptr: return_ptr,
            return_to_block: return_to_block,
            locals: locals?,
            var_offset: num_args,
            temp_offset: num_args + num_vars,
            span: span,
            def_id: def_id,
            substs: substs,
            stmt: 0,
        });
        if self.stack.len() > self.stack_limit {
            Err(EvalError::StackFrameLimitReached)
        } else {
            Ok(())
        }
    }

    fn pop_stack_frame(&mut self) -> EvalResult<'tcx, ()> {
        ::log_settings::settings().indentation -= 1;
        let frame = self.stack.pop().expect("tried to pop a stack frame, but there were none");
        match frame.return_to_block {
            StackPopCleanup::Freeze(alloc_id) => self.memory.freeze(alloc_id)?,
            StackPopCleanup::Goto(target) => self.goto_block(target),
            StackPopCleanup::None => {},
        }
        // TODO(solson): Deallocate local variables.
        Ok(())
    }

    /// Applies the binary operation `op` to the two operands and writes a tuple of the result
    /// and a boolean signifying the potential overflow to the destination.
    fn intrinsic_with_overflow(
        &mut self,
        op: mir::BinOp,
        left: &mir::Operand<'tcx>,
        right: &mir::Operand<'tcx>,
        dest: Pointer,
        dest_layout: &'tcx Layout,
    ) -> EvalResult<'tcx, ()> {
        use rustc::ty::layout::Layout::*;
        let tup_layout = match *dest_layout {
            Univariant { ref variant, .. } => variant,
            _ => bug!("checked bin op returns something other than a tuple"),
        };

        let overflowed = self.intrinsic_overflowing(op, left, right, dest)?;
        let offset = tup_layout.field_offset(1).bytes() as isize;
        self.memory.write_bool(dest.offset(offset), overflowed)
    }

    /// Applies the binary operation `op` to the arguments and writes the result to the destination.
    /// Returns `true` if the operation overflowed.
    fn intrinsic_overflowing(
        &mut self,
        op: mir::BinOp,
        left: &mir::Operand<'tcx>,
        right: &mir::Operand<'tcx>,
        dest: Pointer,
    ) -> EvalResult<'tcx, bool> {
        let left_ptr = self.eval_operand(left)?;
        let left_ty = self.operand_ty(left);
        let left_val = self.read_primval(left_ptr, left_ty)?;

        let right_ptr = self.eval_operand(right)?;
        let right_ty = self.operand_ty(right);
        let right_val = self.read_primval(right_ptr, right_ty)?;

        let (val, overflow) = primval::binary_op(op, left_val, right_val)?;
        self.memory.write_primval(dest, val)?;
        Ok(overflow)
    }

    fn assign_fields<I: IntoIterator<Item = u64>>(
        &mut self,
        dest: Pointer,
        offsets: I,
        operands: &[mir::Operand<'tcx>],
    ) -> EvalResult<'tcx, ()> {
        for (offset, operand) in offsets.into_iter().zip(operands) {
            let src = self.eval_operand(operand)?;
            let src_ty = self.operand_ty(operand);
            let field_dest = dest.offset(offset as isize);
            self.move_(src, field_dest, src_ty)?;
        }
        Ok(())
    }

    fn eval_assignment(&mut self, lvalue: &mir::Lvalue<'tcx>, rvalue: &mir::Rvalue<'tcx>)
        -> EvalResult<'tcx, ()>
    {
        let dest = self.eval_lvalue(lvalue)?.to_ptr();
        let dest_ty = self.lvalue_ty(lvalue);
        let dest_layout = self.type_layout(dest_ty);

        use rustc::mir::repr::Rvalue::*;
        match *rvalue {
            Use(ref operand) => {
                let src = self.eval_operand(operand)?;
                self.move_(src, dest, dest_ty)?;
            }

            BinaryOp(bin_op, ref left, ref right) => {
                // ignore overflow bit, rustc inserts check branches for us
                self.intrinsic_overflowing(bin_op, left, right, dest)?;
            }

            CheckedBinaryOp(bin_op, ref left, ref right) => {
                self.intrinsic_with_overflow(bin_op, left, right, dest, dest_layout)?;
            }

            UnaryOp(un_op, ref operand) => {
                let ptr = self.eval_operand(operand)?;
                let ty = self.operand_ty(operand);
                let val = self.read_primval(ptr, ty)?;
                self.memory.write_primval(dest, primval::unary_op(un_op, val)?)?;
            }

            Aggregate(ref kind, ref operands) => {
                use rustc::ty::layout::Layout::*;
                match *dest_layout {
                    Univariant { ref variant, .. } => {
                        let offsets = iter::once(0)
                            .chain(variant.offset_after_field.iter().map(|s| s.bytes()));
                        self.assign_fields(dest, offsets, operands)?;
                    }

                    Array { .. } => {
                        let elem_size = match dest_ty.sty {
                            ty::TyArray(elem_ty, _) => self.type_size(elem_ty) as u64,
                            _ => bug!("tried to assign {:?} to non-array type {:?}", kind, dest_ty),
                        };
                        let offsets = (0..).map(|i| i * elem_size);
                        self.assign_fields(dest, offsets, operands)?;
                    }

                    General { discr, ref variants, .. } => {
                        if let mir::AggregateKind::Adt(adt_def, variant, _, _) = *kind {
                            let discr_val = adt_def.variants[variant].disr_val.to_u64_unchecked();
                            let discr_size = discr.size().bytes() as usize;
                            self.memory.write_uint(dest, discr_val, discr_size)?;

                            let offsets = variants[variant].offset_after_field.iter()
                                .map(|s| s.bytes());
                            self.assign_fields(dest, offsets, operands)?;
                        } else {
                            bug!("tried to assign {:?} to Layout::General", kind);
                        }
                    }

                    RawNullablePointer { nndiscr, .. } => {
                        if let mir::AggregateKind::Adt(_, variant, _, _) = *kind {
                            if nndiscr == variant as u64 {
                                assert_eq!(operands.len(), 1);
                                let operand = &operands[0];
                                let src = self.eval_operand(operand)?;
                                let src_ty = self.operand_ty(operand);
                                self.move_(src, dest, src_ty)?;
                            } else {
                                assert_eq!(operands.len(), 0);
                                self.memory.write_isize(dest, 0)?;
                            }
                        } else {
                            bug!("tried to assign {:?} to Layout::RawNullablePointer", kind);
                        }
                    }

                    StructWrappedNullablePointer { nndiscr, ref nonnull, ref discrfield } => {
                        if let mir::AggregateKind::Adt(_, variant, _, _) = *kind {
                            if nndiscr == variant as u64 {
                                let offsets = iter::once(0)
                                    .chain(nonnull.offset_after_field.iter().map(|s| s.bytes()));
                                try!(self.assign_fields(dest, offsets, operands));
                            } else {
                                assert_eq!(operands.len(), 0);
                                let offset = self.nonnull_offset(dest_ty, nndiscr, discrfield)?;
                                let dest = dest.offset(offset.bytes() as isize);
                                try!(self.memory.write_isize(dest, 0));
                            }
                        } else {
                            bug!("tried to assign {:?} to Layout::RawNullablePointer", kind);
                        }
                    }

                    CEnum { discr, signed, .. } => {
                        assert_eq!(operands.len(), 0);
                        if let mir::AggregateKind::Adt(adt_def, variant, _, _) = *kind {
                            let val = adt_def.variants[variant].disr_val.to_u64_unchecked();
                            let size = discr.size().bytes() as usize;

                            if signed {
                                self.memory.write_int(dest, val as i64, size)?;
                            } else {
                                self.memory.write_uint(dest, val, size)?;
                            }
                        } else {
                            bug!("tried to assign {:?} to Layout::CEnum", kind);
                        }
                    }

                    _ => return Err(EvalError::Unimplemented(format!("can't handle destination layout {:?} when assigning {:?}", dest_layout, kind))),
                }
            }

            Repeat(ref operand, _) => {
                let (elem_size, elem_align, length) = match dest_ty.sty {
                    ty::TyArray(elem_ty, n) => (self.type_size(elem_ty), self.type_align(elem_ty), n),
                    _ => bug!("tried to assign array-repeat to non-array type {:?}", dest_ty),
                };

                let src = self.eval_operand(operand)?;
                for i in 0..length {
                    let elem_dest = dest.offset((i * elem_size) as isize);
                    self.memory.copy(src, elem_dest, elem_size, elem_align)?;
                }
            }

            Len(ref lvalue) => {
                let src = self.eval_lvalue(lvalue)?;
                let ty = self.lvalue_ty(lvalue);
                let len = match ty.sty {
                    ty::TyArray(_, n) => n as u64,
                    ty::TySlice(_) => if let LvalueExtra::Length(n) = src.extra {
                        n
                    } else {
                        bug!("Rvalue::Len of a slice given non-slice pointer: {:?}", src);
                    },
                    _ => bug!("Rvalue::Len expected array or slice, got {:?}", ty),
                };
                self.memory.write_usize(dest, len)?;
            }

            Ref(_, _, ref lvalue) => {
                let lv = self.eval_lvalue(lvalue)?;
                let (ptr, extra) = self.get_fat_ptr(dest);
                self.memory.write_ptr(ptr, lv.ptr)?;
                match lv.extra {
                    LvalueExtra::None => {},
                    LvalueExtra::Length(len) => {
                        self.memory.write_usize(extra, len)?;
                    }
                    LvalueExtra::Vtable(ptr) => {
                        self.memory.write_ptr(extra, ptr)?;
                    },
                    LvalueExtra::DowncastVariant(..) =>
                        bug!("attempted to take a reference to an enum downcast lvalue"),
                }
            }

            Box(ty) => {
                let size = self.type_size(ty);
                let align = self.type_align(ty);
                let ptr = self.memory.allocate(size, align)?;
                self.memory.write_ptr(dest, ptr)?;
            }

            Cast(kind, ref operand, dest_ty) => {
                use rustc::mir::repr::CastKind::*;
                match kind {
                    Unsize => {
                        let src = self.eval_operand(operand)?;
                        let src_ty = self.operand_ty(operand);
                        let dest_ty = self.monomorphize(dest_ty, self.substs());
                        assert!(self.type_is_fat_ptr(dest_ty));
                        let (ptr, extra) = self.get_fat_ptr(dest);
                        self.move_(src, ptr, src_ty)?;
                        let src_pointee_ty = pointee_type(src_ty).unwrap();
                        let dest_pointee_ty = pointee_type(dest_ty).unwrap();

                        match (&src_pointee_ty.sty, &dest_pointee_ty.sty) {
                            (&ty::TyArray(_, length), &ty::TySlice(_)) => {
                                self.memory.write_usize(extra, length as u64)?;
                            }
                            (&ty::TyTrait(_), &ty::TyTrait(_)) => {
                                // For now, upcasts are limited to changes in marker
                                // traits, and hence never actually require an actual
                                // change to the vtable.
                                let (_, src_extra) = self.get_fat_ptr(src);
                                let src_extra = self.memory.read_ptr(src_extra)?;
                                self.memory.write_ptr(extra, src_extra)?;
                            },
                            (_, &ty::TyTrait(ref data)) => {
                                let trait_ref = data.principal.with_self_ty(self.tcx, src_pointee_ty);
                                let trait_ref = self.tcx.erase_regions(&trait_ref);
                                let vtable = self.get_vtable(trait_ref)?;
                                self.memory.write_ptr(extra, vtable)?;
                            },

                            _ => bug!("invalid unsizing {:?} -> {:?}", src_ty, dest_ty),
                        }
                    }

                    Misc => {
                        let src = self.eval_operand(operand)?;
                        let src_ty = self.operand_ty(operand);
                        if self.type_is_fat_ptr(src_ty) {
                            let (data_ptr, _meta_ptr) = self.get_fat_ptr(src);
                            let ptr_size = self.memory.pointer_size();
                            let dest_ty = self.monomorphize(dest_ty, self.substs());
                            if self.type_is_fat_ptr(dest_ty) {
                                // FIXME: add assertion that the extra part of the src_ty and
                                // dest_ty is of the same type
                                self.memory.copy(data_ptr, dest, ptr_size * 2, ptr_size)?;
                            } else { // cast to thin-ptr
                                // Cast of fat-ptr to thin-ptr is an extraction of data-ptr and
                                // pointer-cast of that pointer to desired pointer type.
                                self.memory.copy(data_ptr, dest, ptr_size, ptr_size)?;
                            }
                        } else {
                            // FIXME: dest_ty should already be monomorphized
                            let dest_ty = self.monomorphize(dest_ty, self.substs());
                            let src_val = self.read_primval(src, src_ty)?;
                            let dest_val = self.cast_primval(src_val, dest_ty)?;
                            self.memory.write_primval(dest, dest_val)?;
                        }
                    }

                    ReifyFnPointer => match self.operand_ty(operand).sty {
                        ty::TyFnDef(def_id, substs, fn_ty) => {
                            let fn_ptr = self.memory.create_fn_ptr(def_id, substs, fn_ty);
                            self.memory.write_ptr(dest, fn_ptr)?;
                        },
                        ref other => bug!("reify fn pointer on {:?}", other),
                    },

                    UnsafeFnPointer => match dest_ty.sty {
                        ty::TyFnPtr(unsafe_fn_ty) => {
                            let src = self.eval_operand(operand)?;
                            let ptr = self.memory.read_ptr(src)?;
                            let (def_id, substs, _) = self.memory.get_fn(ptr.alloc_id)?;
                            let fn_ptr = self.memory.create_fn_ptr(def_id, substs, unsafe_fn_ty);
                            self.memory.write_ptr(dest, fn_ptr)?;
                        },
                        ref other => bug!("fn to unsafe fn cast on {:?}", other),
                    },
                }
            }

            InlineAsm { .. } => unimplemented!(),
        }

        Ok(())
    }

    fn type_is_fat_ptr(&self, ty: Ty<'tcx>) -> bool {
        match ty.sty {
            ty::TyRawPtr(ty::TypeAndMut{ty, ..}) |
            ty::TyRef(_, ty::TypeAndMut{ty, ..}) |
            ty::TyBox(ty) => !self.type_is_sized(ty),
            _ => false,
        }
    }

    fn nonnull_offset(&self, ty: Ty<'tcx>, nndiscr: u64, discrfield: &[u32]) -> EvalResult<'tcx, Size> {
        // Skip the constant 0 at the start meant for LLVM GEP.
        let mut path = discrfield.iter().skip(1).map(|&i| i as usize);

        // Handle the field index for the outer non-null variant.
        let inner_ty = match ty.sty {
            ty::TyEnum(adt_def, substs) => {
                let variant = &adt_def.variants[nndiscr as usize];
                let index = path.next().unwrap();
                let field = &variant.fields[index];
                field.ty(self.tcx, substs)
            }
            _ => bug!("non-enum for StructWrappedNullablePointer: {}", ty),
        };

        self.field_path_offset(inner_ty, path)
    }

    fn field_path_offset<I: Iterator<Item = usize>>(&self, mut ty: Ty<'tcx>, path: I) -> EvalResult<'tcx, Size> {
        let mut offset = Size::from_bytes(0);

        // Skip the initial 0 intended for LLVM GEP.
        for field_index in path {
            let field_offset = self.get_field_offset(ty, field_index)?;
            ty = self.get_field_ty(ty, field_index)?;
            offset = offset.checked_add(field_offset, &self.tcx.data_layout).unwrap();
        }

        Ok(offset)
    }

    fn get_field_ty(&self, ty: Ty<'tcx>, field_index: usize) -> EvalResult<'tcx, Ty<'tcx>> {
        match ty.sty {
            ty::TyStruct(adt_def, substs) => {
                Ok(adt_def.struct_variant().fields[field_index].ty(self.tcx, substs))
            }

            ty::TyRef(_, ty::TypeAndMut { ty, .. }) |
            ty::TyRawPtr(ty::TypeAndMut { ty, .. }) |
            ty::TyBox(ty) => {
                assert_eq!(field_index, 0);
                Ok(ty)
            }
            _ => Err(EvalError::Unimplemented(format!("can't handle type: {:?}", ty))),
        }
    }

    fn get_field_offset(&self, ty: Ty<'tcx>, field_index: usize) -> EvalResult<'tcx, Size> {
        let layout = self.type_layout(ty);

        use rustc::ty::layout::Layout::*;
        match *layout {
            Univariant { .. } => {
                assert_eq!(field_index, 0);
                Ok(Size::from_bytes(0))
            }
            FatPointer { .. } => {
                let bytes = layout::FAT_PTR_ADDR * self.memory.pointer_size();
                Ok(Size::from_bytes(bytes as u64))
            }
            _ => Err(EvalError::Unimplemented(format!("can't handle type: {:?}, with layout: {:?}", ty, layout))),
        }
    }

    fn eval_operand(&mut self, op: &mir::Operand<'tcx>) -> EvalResult<'tcx, Pointer> {
        use rustc::mir::repr::Operand::*;
        match *op {
            Consume(ref lvalue) => Ok(self.eval_lvalue(lvalue)?.to_ptr()),
            Constant(mir::Constant { ref literal, ty, .. }) => {
                use rustc::mir::repr::Literal::*;
                match *literal {
                    Value { ref value } => Ok(self.const_to_ptr(value)?),
                    Item { def_id, substs } => {
                        if let ty::TyFnDef(..) = ty.sty {
                            // function items are zero sized
                            Ok(self.memory.allocate(0, 0)?)
                        } else {
                            let cid = ConstantId {
                                def_id: def_id,
                                substs: substs,
                                kind: ConstantKind::Global,
                            };
                            Ok(*self.statics.get(&cid).expect("static should have been cached (rvalue)"))
                        }
                    },
                    Promoted { index } => {
                        let cid = ConstantId {
                            def_id: self.frame().def_id,
                            substs: self.substs(),
                            kind: ConstantKind::Promoted(index),
                        };
                        Ok(*self.statics.get(&cid).expect("a promoted constant hasn't been precomputed"))
                    },
                }
            }
        }
    }

    fn eval_lvalue(&mut self, lvalue: &mir::Lvalue<'tcx>) -> EvalResult<'tcx, Lvalue> {
        use rustc::mir::repr::Lvalue::*;
        let ptr = match *lvalue {
            ReturnPointer => self.frame().return_ptr
                .expect("ReturnPointer used in a function with no return value"),
            Arg(i) => self.frame().locals[i.index()],
            Var(i) => self.frame().locals[self.frame().var_offset + i.index()],
            Temp(i) => self.frame().locals[self.frame().temp_offset + i.index()],

            Static(def_id) => {
                let substs = subst::Substs::empty(self.tcx);
                let cid = ConstantId {
                    def_id: def_id,
                    substs: substs,
                    kind: ConstantKind::Global,
                };
                *self.statics.get(&cid).expect("static should have been cached (lvalue)")
            },

            Projection(ref proj) => {
                let base = self.eval_lvalue(&proj.base)?;
                let base_ty = self.lvalue_ty(&proj.base);
                let base_layout = self.type_layout(base_ty);

                use rustc::mir::repr::ProjectionElem::*;
                match proj.elem {
                    Field(field, _) => {
                        use rustc::ty::layout::Layout::*;
                        let variant = match *base_layout {
                            Univariant { ref variant, .. } => variant,
                            General { ref variants, .. } => {
                                if let LvalueExtra::DowncastVariant(variant_idx) = base.extra {
                                    &variants[variant_idx]
                                } else {
                                    bug!("field access on enum had no variant index");
                                }
                            }
                            RawNullablePointer { .. } => {
                                assert_eq!(field.index(), 0);
                                return Ok(base);
                            }
                            StructWrappedNullablePointer { ref nonnull, .. } => nonnull,
                            _ => bug!("field access on non-product type: {:?}", base_layout),
                        };

                        let offset = variant.field_offset(field.index()).bytes();
                        base.ptr.offset(offset as isize)
                    },

                    Downcast(_, variant) => {
                        use rustc::ty::layout::Layout::*;
                        match *base_layout {
                            General { discr, .. } => {
                                return Ok(Lvalue {
                                    ptr: base.ptr.offset(discr.size().bytes() as isize),
                                    extra: LvalueExtra::DowncastVariant(variant),
                                });
                            }
                            RawNullablePointer { .. } | StructWrappedNullablePointer { .. } => {
                                return Ok(base);
                            }
                            _ => bug!("variant downcast on non-aggregate: {:?}", base_layout),
                        }
                    },

                    Deref => {
                        let pointee_ty = pointee_type(base_ty).expect("Deref of non-pointer");
                        let ptr = self.memory.read_ptr(base.ptr)?;
                        let extra = match pointee_ty.sty {
                            ty::TySlice(_) | ty::TyStr => {
                                let (_, extra) = self.get_fat_ptr(base.ptr);
                                let len = self.memory.read_usize(extra)?;
                                LvalueExtra::Length(len)
                            }
                            ty::TyTrait(_) => {
                                let (_, extra) = self.get_fat_ptr(base.ptr);
                                let vtable = self.memory.read_ptr(extra)?;
                                LvalueExtra::Vtable(vtable)
                            },
                            _ => LvalueExtra::None,
                        };
                        return Ok(Lvalue { ptr: ptr, extra: extra });
                    }

                    Index(ref operand) => {
                        let elem_size = match base_ty.sty {
                            ty::TyArray(elem_ty, _) |
                            ty::TySlice(elem_ty) => self.type_size(elem_ty),
                            _ => bug!("indexing expected an array or slice, got {:?}", base_ty),
                        };
                        let n_ptr = self.eval_operand(operand)?;
                        let n = self.memory.read_usize(n_ptr)?;
                        base.ptr.offset(n as isize * elem_size as isize)
                    }

                    ConstantIndex { .. } => unimplemented!(),
                    Subslice { .. } => unimplemented!(),
                }
            }
        };

        Ok(Lvalue { ptr: ptr, extra: LvalueExtra::None })
    }

    fn get_fat_ptr(&self, ptr: Pointer) -> (Pointer, Pointer) {
        assert_eq!(layout::FAT_PTR_ADDR, 0);
        assert_eq!(layout::FAT_PTR_EXTRA, 1);
        (ptr, ptr.offset(self.memory.pointer_size() as isize))
    }

    fn lvalue_ty(&self, lvalue: &mir::Lvalue<'tcx>) -> Ty<'tcx> {
        self.monomorphize(lvalue.ty(&self.mir(), self.tcx).to_ty(self.tcx), self.substs())
    }

    fn operand_ty(&self, operand: &mir::Operand<'tcx>) -> Ty<'tcx> {
        self.monomorphize(operand.ty(&self.mir(), self.tcx), self.substs())
    }

    fn move_(&mut self, src: Pointer, dest: Pointer, ty: Ty<'tcx>) -> EvalResult<'tcx, ()> {
        let size = self.type_size(ty);
        let align = self.type_align(ty);
        self.memory.copy(src, dest, size, align)?;
        Ok(())
    }

    pub fn read_primval(&mut self, ptr: Pointer, ty: Ty<'tcx>) -> EvalResult<'tcx, PrimVal> {
        use syntax::ast::{IntTy, UintTy, FloatTy};
        let val = match (self.memory.pointer_size(), &ty.sty) {
            (_, &ty::TyBool)              => PrimVal::Bool(self.memory.read_bool(ptr)?),
            (_, &ty::TyChar)              => {
                let c = self.memory.read_uint(ptr, 4)? as u32;
                match ::std::char::from_u32(c) {
                    Some(ch) => PrimVal::Char(ch),
                    None => return Err(EvalError::InvalidChar(c as u64)),
                }
            }
            (_, &ty::TyInt(IntTy::I8))    => PrimVal::I8(self.memory.read_int(ptr, 1)? as i8),
            (2, &ty::TyInt(IntTy::Is)) |
            (_, &ty::TyInt(IntTy::I16))   => PrimVal::I16(self.memory.read_int(ptr, 2)? as i16),
            (4, &ty::TyInt(IntTy::Is)) |
            (_, &ty::TyInt(IntTy::I32))   => PrimVal::I32(self.memory.read_int(ptr, 4)? as i32),
            (8, &ty::TyInt(IntTy::Is)) |
            (_, &ty::TyInt(IntTy::I64))   => PrimVal::I64(self.memory.read_int(ptr, 8)? as i64),
            (_, &ty::TyUint(UintTy::U8))  => PrimVal::U8(self.memory.read_uint(ptr, 1)? as u8),
            (2, &ty::TyUint(UintTy::Us)) |
            (_, &ty::TyUint(UintTy::U16)) => PrimVal::U16(self.memory.read_uint(ptr, 2)? as u16),
            (4, &ty::TyUint(UintTy::Us)) |
            (_, &ty::TyUint(UintTy::U32)) => PrimVal::U32(self.memory.read_uint(ptr, 4)? as u32),
            (8, &ty::TyUint(UintTy::Us)) |
            (_, &ty::TyUint(UintTy::U64)) => PrimVal::U64(self.memory.read_uint(ptr, 8)? as u64),

            (_, &ty::TyFloat(FloatTy::F32)) => PrimVal::F32(self.memory.read_f32(ptr)?),
            (_, &ty::TyFloat(FloatTy::F64)) => PrimVal::F64(self.memory.read_f64(ptr)?),

            (_, &ty::TyFnDef(def_id, substs, fn_ty)) => {
                PrimVal::FnPtr(self.memory.create_fn_ptr(def_id, substs, fn_ty))
            },
            (_, &ty::TyFnPtr(_)) => self.memory.read_ptr(ptr).map(PrimVal::FnPtr)?,
            (_, &ty::TyRef(_, ty::TypeAndMut { ty, .. })) |
            (_, &ty::TyRawPtr(ty::TypeAndMut { ty, .. })) => {
                if self.type_is_sized(ty) {
                    match self.memory.read_ptr(ptr) {
                        Ok(p) => PrimVal::AbstractPtr(p),
                        Err(EvalError::ReadBytesAsPointer) => {
                            PrimVal::IntegerPtr(self.memory.read_usize(ptr)?)
                        }
                        Err(e) => return Err(e),
                    }
                } else {
                    return Err(EvalError::Unimplemented(format!("unimplemented: primitive read of fat pointer type: {:?}", ty)));
                }
            }

            (_, &ty::TyEnum(..)) => {
                use rustc::ty::layout::Layout::*;
                if let CEnum { discr, signed, .. } = *self.type_layout(ty) {
                    match (discr.size().bytes(), signed) {
                        (1, true)  => PrimVal::I8(self.memory.read_int(ptr, 1)? as i8),
                        (2, true)  => PrimVal::I16(self.memory.read_int(ptr, 2)? as i16),
                        (4, true)  => PrimVal::I32(self.memory.read_int(ptr, 4)? as i32),
                        (8, true)  => PrimVal::I64(self.memory.read_int(ptr, 8)? as i64),
                        (1, false) => PrimVal::U8(self.memory.read_uint(ptr, 1)? as u8),
                        (2, false) => PrimVal::U16(self.memory.read_uint(ptr, 2)? as u16),
                        (4, false) => PrimVal::U32(self.memory.read_uint(ptr, 4)? as u32),
                        (8, false) => PrimVal::U64(self.memory.read_uint(ptr, 8)? as u64),
                        (size, _) => bug!("CEnum discr size {}", size),
                    }
                } else {
                    bug!("primitive read of non-clike enum: {:?}", ty);
                }
            },

            _ => bug!("primitive read of non-primitive type: {:?}", ty),
        };
        Ok(val)
    }

    fn frame(&self) -> &Frame<'a, 'tcx> {
        self.stack.last().expect("no call frames exist")
    }

    pub fn frame_mut(&mut self) -> &mut Frame<'a, 'tcx> {
        self.stack.last_mut().expect("no call frames exist")
    }

    fn mir(&self) -> CachedMir<'a, 'tcx> {
        self.frame().mir.clone()
    }

    fn substs(&self) -> &'tcx Substs<'tcx> {
        self.frame().substs
    }
}

fn pointee_type(ptr_ty: ty::Ty) -> Option<ty::Ty> {
    match ptr_ty.sty {
        ty::TyRef(_, ty::TypeAndMut { ty, .. }) |
        ty::TyRawPtr(ty::TypeAndMut { ty, .. }) |
        ty::TyBox(ty) => {
            Some(ty)
        }
        _ => None,
    }
}

impl Lvalue {
    fn to_ptr(self) -> Pointer {
        assert_eq!(self.extra, LvalueExtra::None);
        self.ptr
    }
}

impl<'mir, 'tcx: 'mir> Deref for CachedMir<'mir, 'tcx> {
    type Target = mir::Mir<'tcx>;
    fn deref(&self) -> &mir::Mir<'tcx> {
        match *self {
            CachedMir::Ref(r) => r,
            CachedMir::Owned(ref rc) => rc,
        }
    }
}

pub fn eval_main<'a, 'tcx: 'a>(
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
    mir_map: &'a MirMap<'tcx>,
    def_id: DefId,
    memory_size: usize,
    step_limit: u64,
    stack_limit: usize,
) {
    let mir = mir_map.map.get(&def_id).expect("no mir for main function");
    let mut ecx = EvalContext::new(tcx, mir_map, memory_size, stack_limit);
    let substs = subst::Substs::empty(tcx);
    let return_ptr = ecx.alloc_ret_ptr(mir.return_ty, substs)
        .expect("should at least be able to allocate space for the main function's return value");

    ecx.push_stack_frame(def_id, mir.span, CachedMir::Ref(mir), substs, Some(return_ptr), StackPopCleanup::None)
        .expect("could not allocate first stack frame");

    if mir.arg_decls.len() == 2 {
        // start function
        let ptr_size = ecx.memory().pointer_size();
        let nargs = ecx.memory_mut().allocate(ptr_size, ptr_size).expect("can't allocate memory for nargs");
        ecx.memory_mut().write_usize(nargs, 0).unwrap();
        let args = ecx.memory_mut().allocate(ptr_size, ptr_size).expect("can't allocate memory for arg pointer");
        ecx.memory_mut().write_usize(args, 0).unwrap();
        ecx.frame_mut().locals[0] = nargs;
        ecx.frame_mut().locals[1] = args;
    }

    for _ in 0..step_limit {
        match ecx.step() {
            Ok(true) => {}
            Ok(false) => return,
            // FIXME: diverging functions can end up here in some future miri
            Err(e) => {
                report(tcx, &ecx, e);
                return;
            }
        }
    }
    report(tcx, &ecx, EvalError::ExecutionTimeLimitReached);
}

fn report(tcx: TyCtxt, ecx: &EvalContext, e: EvalError) {
    let frame = ecx.stack().last().expect("stackframe was empty");
    let block = &frame.mir.basic_blocks()[frame.block];
    let span = if frame.stmt < block.statements.len() {
        block.statements[frame.stmt].source_info.span
    } else {
        block.terminator().source_info.span
    };
    let mut err = tcx.sess.struct_span_err(span, &e.to_string());
    for &Frame { def_id, substs, span, .. } in ecx.stack().iter().rev() {
        // FIXME(solson): Find a way to do this without this Display impl hack.
        use rustc::util::ppaux;
        use std::fmt;
        struct Instance<'tcx>(DefId, &'tcx subst::Substs<'tcx>);
        impl<'tcx> fmt::Display for Instance<'tcx> {
            fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
                ppaux::parameterized(f, self.1, self.0, ppaux::Ns::Value, &[])
            }
        }
        err.span_note(span, &format!("inside call to {}", Instance(def_id, substs)));
    }
    err.emit();
}

pub fn run_mir_passes<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>, mir_map: &mut MirMap<'tcx>) {
    let mut passes = ::rustc::mir::transform::Passes::new();
    passes.push_hook(Box::new(::rustc_mir::transform::dump_mir::DumpMir));
    passes.push_pass(Box::new(::rustc_mir::transform::no_landing_pads::NoLandingPads));
    passes.push_pass(Box::new(::rustc_mir::transform::simplify_cfg::SimplifyCfg::new("no-landing-pads")));

    passes.push_pass(Box::new(::rustc_mir::transform::erase_regions::EraseRegions));

    passes.push_pass(Box::new(::rustc_borrowck::ElaborateDrops));
    passes.push_pass(Box::new(::rustc_mir::transform::no_landing_pads::NoLandingPads));
    passes.push_pass(Box::new(::rustc_mir::transform::simplify_cfg::SimplifyCfg::new("elaborate-drops")));
    passes.push_pass(Box::new(::rustc_mir::transform::dump_mir::Marker("PreMiri")));

    passes.run_passes(tcx, mir_map);
}

// TODO(solson): Upstream these methods into rustc::ty::layout.

trait IntegerExt {
    fn size(self) -> Size;
}

impl IntegerExt for layout::Integer {
    fn size(self) -> Size {
        use rustc::ty::layout::Integer::*;
        match self {
            I1 | I8 => Size::from_bits(8),
            I16 => Size::from_bits(16),
            I32 => Size::from_bits(32),
            I64 => Size::from_bits(64),
        }
    }
}

trait StructExt {
    fn field_offset(&self, index: usize) -> Size;
}

impl StructExt for layout::Struct {
    fn field_offset(&self, index: usize) -> Size {
        if index == 0 {
            Size::from_bytes(0)
        } else {
            self.offset_after_field[index - 1]
        }
    }
}
