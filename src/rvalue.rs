use {Lvalue, Value, EvalContext};

pub enum Rvalue<'tcx> {
    Lvalue(Lvalue<'tcx>),
    Value(Value),
}

impl<'a, 'tcx> EvalContext<'a, 'tcx> {
    pub fn read_rvalue(&self, rval: Rvalue<'tcx>) -> Value {
        match rval {
            Rvalue::Lvalue(lval) => self.read_lvalue(lval),
            Rvalue::Value(val) => val,
        }
    }
}
