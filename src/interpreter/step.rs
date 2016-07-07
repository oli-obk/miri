//! This module contains the `EvalContext` methods for executing a single step of the interpreter.
//!
//! The main entry point is the `step` method.

use super::{
    CachedMir,
    ConstantId,
    EvalContext,
    ConstantKind,
};
use error::EvalResult;
use rustc::mir::repr as mir;
use rustc::ty::{subst, self};
use rustc::hir::def_id::DefId;
use rustc::mir::visit::{Visitor, LvalueContext};
use syntax::codemap::Span;
use std::rc::Rc;

impl<'a, 'tcx> EvalContext<'a, 'tcx> {
    /// Returns true as long as there are more things to do.
    pub fn step(&mut self) -> EvalResult<'tcx, bool> {
        if self.stack.is_empty() {
            return Ok(false);
        }

        let block = self.frame().block;
        let stmt = self.frame().stmt;
        let mir = self.mir();
        let basic_block = &mir.basic_blocks()[block];

        if let Some(ref stmt) = basic_block.statements.get(stmt) {
            let mut new = Ok(0);
            ConstantExtractor {
                span: stmt.source_info.span,
                substs: self.substs(),
                def_id: self.frame().def_id,
                ecx: self,
                mir: &mir,
                new_constants: &mut new,
            }.visit_statement(block, stmt);
            if new? == 0 {
                self.statement(stmt)?;
            }
            // if ConstantExtractor added new frames, we don't execute anything here
            // but await the next call to step
            return Ok(true);
        }

        let terminator = basic_block.terminator();
        let mut new = Ok(0);
        ConstantExtractor {
            span: terminator.source_info.span,
            substs: self.substs(),
            def_id: self.frame().def_id,
            ecx: self,
            mir: &mir,
            new_constants: &mut new,
        }.visit_terminator(block, terminator);
        if new? == 0 {
            self.terminator(terminator)?;
        }
        // if ConstantExtractor added new frames, we don't execute anything here
        // but await the next call to step
        Ok(true)
    }

    fn statement(&mut self, stmt: &mir::Statement<'tcx>) -> EvalResult<'tcx, ()> {
        trace!("{:?}", stmt);
        let mir::StatementKind::Assign(ref lvalue, ref rvalue) = stmt.kind;
        self.eval_assignment(lvalue, rvalue)?;
        self.frame_mut().stmt += 1;
        Ok(())
    }

    fn terminator(&mut self, terminator: &mir::Terminator<'tcx>) -> EvalResult<'tcx, ()> {
        trace!("{:?}", terminator.kind);
        self.eval_terminator(terminator)?;
        if !self.stack.is_empty() {
            trace!("// {:?}", self.frame().block);
        }
        Ok(())
    }
}

// WARNING: make sure that any methods implemented on this type don't ever access ecx.stack
// this includes any method that might access the stack
// basically don't call anything other than `load_mir`, `alloc_ret_ptr`, `push_stack_frame`
// The reason for this is, that `push_stack_frame` modifies the stack out of obvious reasons
struct ConstantExtractor<'a, 'b: 'a, 'tcx: 'b> {
    span: Span,
    ecx: &'a mut EvalContext<'b, 'tcx>,
    mir: &'a mir::Mir<'tcx>,
    def_id: DefId,
    substs: &'tcx subst::Substs<'tcx>,
    new_constants: &'a mut EvalResult<'tcx, u64>,
}

impl<'a, 'b, 'tcx> ConstantExtractor<'a, 'b, 'tcx> {
    fn global_item(&mut self, def_id: DefId, substs: &'tcx subst::Substs<'tcx>, span: Span) {
        let cid = ConstantId {
            def_id: def_id,
            substs: substs,
            kind: ConstantKind::Global,
        };
        if self.ecx.statics.contains_key(&cid) {
            return;
        }
        let mir = self.ecx.load_mir(def_id);
        self.try(|this| {
            let ptr = this.ecx.alloc_ret_ptr(mir.return_ty, substs)?;
            let ptr = ptr.expect("there's no such thing as an unreachable static");
            this.ecx.statics.insert(cid.clone(), ptr);
            this.ecx.push_stack_frame(def_id, span, mir, substs, Some(ptr), None)
        });
    }
    fn try<F: FnOnce(&mut Self) -> EvalResult<'tcx, ()>>(&mut self, f: F) {
        if let Ok(ref mut n) = *self.new_constants {
            *n += 1;
        } else {
            return;
        }
        if let Err(e) = f(self) {
            *self.new_constants = Err(e);
        }
    }
}

impl<'a, 'b, 'tcx> Visitor<'tcx> for ConstantExtractor<'a, 'b, 'tcx> {
    fn visit_constant(&mut self, constant: &mir::Constant<'tcx>) {
        self.super_constant(constant);
        match constant.literal {
            // already computed by rustc
            mir::Literal::Value { .. } => {}
            mir::Literal::Item { def_id, substs } => {
                if let ty::TyFnDef(..) = constant.ty.sty {
                    // No need to do anything here,
                    // because the type is the actual function, not the signature of the function.
                    // Thus we can simply create a zero sized allocation in `evaluate_operand`
                } else {
                    self.global_item(def_id, substs, constant.span);
                }
            },
            mir::Literal::Promoted { index } => {
                let cid = ConstantId {
                    def_id: self.def_id,
                    substs: self.substs,
                    kind: ConstantKind::Promoted(index),
                };
                if self.ecx.statics.contains_key(&cid) {
                    return;
                }
                let mir = self.mir.promoted[index].clone();
                let return_ty = mir.return_ty;
                self.try(|this| {
                    let return_ptr = this.ecx.alloc_ret_ptr(return_ty, cid.substs)?;
                    let return_ptr = return_ptr.expect("there's no such thing as an unreachable static");
                    let mir = CachedMir::Owned(Rc::new(mir));
                    this.ecx.statics.insert(cid.clone(), return_ptr);
                    this.ecx.push_stack_frame(this.def_id, constant.span, mir, this.substs, Some(return_ptr), None)
                });
            }
        }
    }

    fn visit_lvalue(&mut self, lvalue: &mir::Lvalue<'tcx>, context: LvalueContext) {
        self.super_lvalue(lvalue, context);
        if let mir::Lvalue::Static(def_id) = *lvalue {
            let substs = self.ecx.tcx.mk_substs(subst::Substs::empty());
            let span = self.span;
            self.global_item(def_id, substs, span);
        }
    }
}
