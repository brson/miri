use libc::c_char;
use error::*;
use rustc::mir::repr::*;
use rustc::mir::mir_map::MirMap;
use rustc::middle::const_val::ConstVal;
use rustc_const_math::{ConstInt, ConstIsize};
use rustc::ty::{self, TyCtxt, Ty, FnSig};
use rustc::hir::intravisit::{self, Visitor, FnKind};
use rustc::hir::{FnDecl, Block};
use rustc::hir::def_id::DefId;
use syntax::ast::{NodeId, IntTy, UintTy, FloatTy};
use syntax::codemap::Span;
use std::ffi::CString;
use std::ptr;
use std::collections::HashMap;
use binaryen::*;
use monomorphize;
use traits;

pub fn trans_crate<'a, 'tcx>(tcx: &TyCtxt<'a, 'tcx, 'tcx>,
                             mir_map: &MirMap<'tcx>,
                             entry_fn: Option<NodeId>) -> Result<()> {

    let _ignore = tcx.dep_graph.in_ignore();

    let ref mut v = BinaryenModuleCtxt {
        tcx: tcx,
        mir_map: mir_map,
        module: unsafe { BinaryenModuleCreate() },
        fun_types: HashMap::new(),
        fun_names: HashMap::new(),
        fun_refs: HashMap::new(),
        c_strings: Vec::new(),
    };

    tcx.map.krate().visit_all_items(v);

    unsafe {
        // export #[start] or #[main] entry fn when available
        if let Some(entry_fn) = entry_fn {
            let entry_fn_did = tcx.map.local_def_id(entry_fn);
            for (&(did, ref sig), name) in &v.fun_names {
                if entry_fn_did == did {
                    debug!("emitting Export for entry fn {:?}", name);
                    BinaryenAddExport(v.module, name.as_ptr(), name.as_ptr());

                    let mir = mir_map.map.get(&entry_fn).expect("no mir for main function");
                    if mir.arg_decls.len() == 0 {
                        // we have a #[main] fn, set it as the module start method
                        debug!("emitting SetStart for #[main] fn {:?}", name);
                        let module_start_fn = v.fun_refs[&(did, sig.clone())];
                        BinaryenSetStart(v.module, module_start_fn);
                    }
                }
            }
        }

        // TODO: validate the module
        // TODO: make it possible to print the optimized module
        BinaryenModulePrint(v.module);
    }

    Ok(())
}

struct BinaryenModuleCtxt<'v, 'tcx: 'v> {
    tcx: &'v TyCtxt<'v, 'tcx, 'tcx>,
    mir_map: &'v MirMap<'tcx>,
    module: BinaryenModuleRef,
    fun_types: HashMap<ty::FnSig<'tcx>, BinaryenFunctionTypeRef>,
    fun_names: HashMap<(DefId, ty::FnSig<'tcx>), CString>,
    fun_refs: HashMap<(DefId, ty::FnSig<'tcx>), BinaryenFunctionRef>,
    c_strings: Vec<CString>,
}

impl<'v, 'tcx: 'v> Drop for BinaryenModuleCtxt<'v, 'tcx> {
    fn drop(&mut self) {
        unsafe { BinaryenModuleDispose(self.module) };
    }
}

impl<'v, 'tcx> Visitor<'v> for BinaryenModuleCtxt<'v, 'tcx> {
    fn visit_fn(&mut self, fk: FnKind<'v>, fd: &'v FnDecl,
                b: &'v Block, s: Span, id: NodeId) {
        let did = self.tcx.map.local_def_id(id);
        let type_scheme = self.tcx.lookup_item_type(did);

        // don't translate generic functions yet
        if !type_scheme.generics.types.is_empty() {
            return;
        }

        let mir = &self.mir_map.map[&id];
        let sig = type_scheme.ty.fn_sig().skip_binder();

        {
            let mut ctxt = BinaryenFnCtxt {
                tcx: self.tcx,
                mir_map: self.mir_map,
                mir: mir,
                did: did,
                sig: &sig,
                module: self.module,
                fun_types: &mut self.fun_types,
                fun_names: &mut self.fun_names,
                fun_refs: &mut self.fun_refs,
                c_strings: &mut self.c_strings,
            };

            ctxt.trans();
        }

        intravisit::walk_fn(self, fk, fd, b, s)
    }
}

struct BinaryenFnCtxt<'v, 'tcx: 'v> {
    tcx: &'v TyCtxt<'v, 'tcx, 'tcx>,
    mir_map: &'v MirMap<'tcx>,
    mir: &'v Mir<'tcx>,
    did: DefId,
    sig: &'v FnSig<'tcx>,
    module: BinaryenModuleRef,
    fun_types: &'v mut HashMap<ty::FnSig<'tcx>, BinaryenFunctionTypeRef>,
    fun_names: &'v mut HashMap<(DefId, ty::FnSig<'tcx>), CString>,
    fun_refs: &'v mut HashMap<(DefId, ty::FnSig<'tcx>), BinaryenFunctionRef>,
    c_strings: &'v mut Vec<CString>,
}

#[derive(Debug)]
enum BinaryenCallKind {
    Direct,
    Import,
    // Indirect // unimplemented at the moment
}

impl<'v, 'tcx: 'v> BinaryenFnCtxt<'v, 'tcx> {
    /// This is the main entry point for MIR->wasm fn translation
    fn trans(&mut self) {

        // Maintain a cache of translated monomorphizations and bail
        // if we've already seen this one.
        let fn_name_ptr;
        if self.fun_names.contains_key(&(self.did, self.sig.clone())) {
            return;
        } else {
            let fn_name = sanitize_symbol(&self.tcx.item_path_str(self.did));
            let fn_name = CString::new(fn_name).expect("");
            fn_name_ptr = fn_name.as_ptr();
            self.fun_names.insert((self.did, self.sig.clone()), fn_name);
        }

        debug!("translating fn {:?}", self.tcx.item_path_str(self.did));

        // Translate arg and ret tys to wasm
        let binaryen_args: Vec<_> = self.sig.inputs.iter().map(|t| rust_ty_to_binaryen(t)).collect();
        let mut needs_ret_local = false;
        let binaryen_ret = match self.sig.output {
            ty::FnOutput::FnConverging(t) => {
                if !t.is_nil() {
                    needs_ret_local = true;
                    rust_ty_to_binaryen(t)
                } else {
                    BinaryenNone()
                }
            },
            ty::FnOutput::FnDiverging => {
                BinaryenNone()
            }
        };

        // Create the wasm locals
        let mut locals = Vec::new();

        for mir_local in &self.mir.var_decls {
            locals.push(rust_ty_to_binaryen(mir_local.ty));
        }

        for mir_local in &self.mir.temp_decls {
            locals.push(rust_ty_to_binaryen(mir_local.ty));
        }

        if needs_ret_local {
            locals.push(binaryen_ret);
        }

        // reelooper helper for irreducible control flow
        locals.push(BinaryenInt32());
        let relooper_local = BinaryenIndex(locals.len() as _);

        debug!("{} wasm locals found", locals.len());

        // Create the relooper for tying together basic blocks. We're
        // going to first translate the basic blocks without the
        // terminators, then go back over the basic blocks and use the
        // terminators to configure the relooper.
        let relooper = unsafe { RelooperCreate() };

        let mut relooper_blocks = Vec::new();

        debug!("{} MIR basic blocks to translate", self.mir.basic_blocks.len());

        for (i, bb) in self.mir.basic_blocks.iter().enumerate() {
            debug!("bb{}: {:#?}", i, bb);

            let mut binaryen_stmts = Vec::new();
            for stmt in &bb.statements {
                match stmt.kind {
                    StatementKind::Assign(ref lval, ref rval) => {
                        let (lval_i, _) = self.trans_lval(lval);
                        if let Some(rval_expr) = self.trans_rval(rval) {
                            debug!("emitting SetLocal for Assign '{:?} = {:?}'", lval, rval);
                            let binaryen_stmt = unsafe { BinaryenSetLocal(self.module, lval_i, rval_expr) };
                            binaryen_stmts.push(binaryen_stmt);
                        }
                    }
                }
            }

            // Some features of MIR terminators tranlate to wasm
            // expressions, some translate to relooper edges. These
            // are the expressions.
            match bb.terminator().kind {
                TerminatorKind::Return => {
                    debug!("emitting Return from fn {:?}", self.tcx.item_path_str(self.did));
                    let expr = self.trans_operand(&Operand::Consume(Lvalue::ReturnPointer));
                    let expr = unsafe { BinaryenReturn(self.module, expr) };
                    binaryen_stmts.push(expr);
                }
                TerminatorKind::Call {
                    ref func, ref args, ref destination, ..
                } => unsafe {
                    if let Some((b_func, b_fnty, call_kind)) = self.trans_fn_name_direct(func) {
                        let b_args: Vec<_> = args.iter().map(|a| self.trans_operand(a)).collect();
                        let b_call = match call_kind {
                            BinaryenCallKind::Direct => {
                                BinaryenCall(self.module,
                                             b_func,
                                             b_args.as_ptr(),
                                             BinaryenIndex(b_args.len() as _),
                                             b_fnty)
                            }
                            BinaryenCallKind::Import => {
                                BinaryenCallImport(self.module,
                                                   b_func,
                                                   b_args.as_ptr(),
                                                   BinaryenIndex(b_args.len() as _),
                                                   b_fnty)
                            }
                        };

                        match *destination {
                            Some((ref lvalue, _)) => {
                                let b_lvalue = self.trans_lval(lvalue);

                                if b_fnty == BinaryenNone() {
                                    // The result of the Rust call is put in MIR into a tmp local,
                                    // but the wasm function returns void (like the print externs)
                                    // TODO: what representation should the unit type have in wasm ?
                                    debug!("emitting {:?} Call to fn {:?} + SetLocal for unit type", call_kind, func);
                                    binaryen_stmts.push(b_call);

                                    let unit_type = BinaryenConst(self.module, BinaryenLiteralInt32(0));
                                    binaryen_stmts.push(BinaryenSetLocal(self.module, b_lvalue.0, unit_type));
                                } else {
                                    debug!("emitting {:?} Call to fn {:?} + SetLocal of the result", call_kind, func);
                                    binaryen_stmts.push(BinaryenSetLocal(self.module, b_lvalue.0, b_call));
                                }
                            }
                            _ => {
                                debug!("emitting Call to fn {:?}", func);
                                binaryen_stmts.push(b_call);
                            }
                        }
                    } else {
                        panic!()
                    }
                },
                _ => ()
            }
            unsafe {
                let name = format!("bb{}", i);
                let name = CString::new(name).expect("");
                let name_ptr = name.as_ptr();
                self.c_strings.push(name);
                let binaryen_expr = BinaryenBlock(self.module, name_ptr,
                                                  binaryen_stmts.as_ptr(),
                                                  BinaryenIndex(binaryen_stmts.len() as _));
                let relooper_block = RelooperAddBlock(relooper, binaryen_expr);
                debug!("emitting {}-statement Block bb{}", binaryen_stmts.len(), i);
                relooper_blocks.push(relooper_block);
            }
        }

        // Create the relooper edges from the bb terminators
        for (i, bb) in self.mir.basic_blocks.iter().enumerate() {
            match bb.terminator().kind {
                TerminatorKind::Goto { ref target } => {
                    debug!("emitting Branch for Goto, from bb{} to bb{}", i, target.index());
                    unsafe {
                        RelooperAddBranch(relooper_blocks[i], relooper_blocks[target.index()],
                                          BinaryenExpressionRef(ptr::null_mut()),
                                          BinaryenExpressionRef(ptr::null_mut()));
                    }
                }
                TerminatorKind::If { ref cond, ref targets } => {
                    debug!("emitting Branches for If, from bb{} to bb{} and bb{}", i, targets.0.index(), targets.1.index());

                    let cond = self.trans_operand(cond);

                    unsafe {
                        RelooperAddBranch(relooper_blocks[i], relooper_blocks[targets.0.index()],
                                          cond,
                                          BinaryenExpressionRef(ptr::null_mut()));
                        RelooperAddBranch(relooper_blocks[i], relooper_blocks[targets.1.index()],
                                          BinaryenExpressionRef(ptr::null_mut()),
                                          BinaryenExpressionRef(ptr::null_mut()));
                    }
                }
                TerminatorKind::Return => {
                    /* handled during bb creation */
                }
                TerminatorKind::Call {
                    ref destination, ref cleanup, ..
                } => {
                    let _ = cleanup; // FIXME
                    match *destination {
                        Some((_, ref target)) => {
                            debug!("emitting Branch for Call, from bb{} to bb{}", i, target.index());
                            unsafe {
                                RelooperAddBranch(relooper_blocks[i], relooper_blocks[target.index()],
                                                  BinaryenExpressionRef(ptr::null_mut()),
                                                  BinaryenExpressionRef(ptr::null_mut()));
                            }
                        }
                        _ => panic!()
                    }
                }
                _ => panic!("unimplemented terminator {:?}", bb.terminator().kind)
            }
        }

        unsafe {
            if !self.fun_types.contains_key(self.sig) {
                let name = format!("rustfn-{}-{}", self.did.krate, self.did.index.as_u32());
                let name = CString::new(name).expect("");
                let name_ptr = name.as_ptr();
                self.c_strings.push(name);
                let ty = BinaryenAddFunctionType(self.module,
                                                 name_ptr,
                                                 binaryen_ret,
                                                 binaryen_args.as_ptr(),
                                                 BinaryenIndex(binaryen_args.len() as _));
                self.fun_types.insert(self.sig.clone(), ty);
            }

            let body = RelooperRenderAndDispose(relooper,
                                                relooper_blocks[0],
                                                relooper_local,
                                                self.module);
            let f = BinaryenAddFunction(self.module, fn_name_ptr,
                                        *self.fun_types.get(self.sig).unwrap(),
                                        locals.as_ptr(),
                                        BinaryenIndex(locals.len() as _),
                                        body);
            self.fun_refs.insert((self.did, self.sig.clone()), f);
        }

        debug!("done translating fn {:?}\n", self.tcx.item_path_str(self.did));
    }

    fn trans_lval(&mut self, lvalue: &Lvalue) -> (BinaryenIndex, u32) {
        let i = match *lvalue {
            Lvalue::Arg(i) => i,
            Lvalue::Var(i) => self.mir.arg_decls.len() as u32 + i,
            Lvalue::Temp(i) => {
                self.mir.arg_decls.len() as u32 +
                    self.mir.var_decls.len() as u32 + i
            }
            Lvalue::ReturnPointer => {
                self.mir.arg_decls.len() as u32 +
                    self.mir.var_decls.len() as u32 +
                    self.mir.temp_decls.len() as u32
            }
            Lvalue::Projection(ref projection) => {
                if projection.elem == ProjectionElem::Deref {
                    let (i, _) = self.trans_lval(&projection.base);
                    i.0
                } else {
                    panic!("unimplemented ProjectionElem: {:?}", projection.elem);
                }
            }
            _ => panic!("unimplemented Lvalue: {:?}", lvalue)
        };

        (BinaryenIndex(i), 0)
    }

    fn trans_rval(&mut self, rvalue: &Rvalue<'tcx>) -> Option<BinaryenExpressionRef> {
        unsafe {
            match *rvalue {
                Rvalue::Use(ref operand) => {
                    Some(self.trans_operand(operand))
                }
                Rvalue::BinaryOp(ref op, ref a, ref b) => {
                    let a = self.trans_operand(a);
                    let b = self.trans_operand(b);
                    // TODO: implement binary ops for other types than just i32s
                    let op = match *op {
                        BinOp::Add => BinaryenAddInt32(),
                        BinOp::Sub => BinaryenSubInt32(),
                        BinOp::Mul => BinaryenMulInt32(),
                        BinOp::Div => BinaryenDivSInt32(),
                        BinOp::Eq => BinaryenEqInt32(),
                        BinOp::Ne => BinaryenNeInt32(),
                        _ => panic!("unimplemented BinOp: {:?}", op)
                    };
                    Some(BinaryenBinary(self.module, op, a, b))
                }
                Rvalue::Ref( _, _, ref lvalue) => {
                    // TODO: for shared refs, similar to Operand::Consume and could be shared
                    let (i, _) = self.trans_lval(lvalue);
                    let lval_ty = self.mir.lvalue_ty(*self.tcx, lvalue);
                    let t = lval_ty.to_ty(*self.tcx);
                    let t = rust_ty_to_binaryen(t);
                    Some(BinaryenGetLocal(self.module, i, t))
                }
                _ => {
                    None
                }
            }
        }
    }

    fn trans_operand(&mut self, operand: &Operand<'tcx>) -> BinaryenExpressionRef {
        match *operand {
            Operand::Consume(ref lvalue) => {
                let (i, _) = self.trans_lval(lvalue);
                let lval_ty = self.mir.lvalue_ty(*self.tcx, lvalue);
                let t = lval_ty.to_ty(*self.tcx);
                let t = rust_ty_to_binaryen(t);
                unsafe { BinaryenGetLocal(self.module, i, t) }
            }
            Operand::Constant(ref c) => {
                match c.literal {
                    Literal::Value { ref value } => {
                        match *value {
                            ConstVal::Integral(ConstInt::Isize(ConstIsize::Is32(val))) |
                            ConstVal::Integral(ConstInt::I32(val)) => {
                                unsafe {
                                    let lit = BinaryenLiteralInt32(val);
                                    BinaryenConst(self.module, lit)
                                }
                            }
                            ConstVal::Bool (val) => {
                                let val = if val { 1 } else { 0 };
                                unsafe {
                                    let lit = BinaryenLiteralInt32(val);
                                    BinaryenConst(self.module, lit)
                                }
                            }
                            _ => panic!("unimplemented value: {:?}", value)
                        }
                    }
                    Literal::Promoted { .. } => {
                        panic!("unimplemented Promoted literal: {:?}", c)
                    }
                    _ => panic!("{:?}", c)
                }
            }
        }
    }

    fn trans_fn_name_direct(&mut self, operand: &Operand<'tcx>) -> Option<(*const c_char, BinaryenType, BinaryenCallKind)> {
        match *operand {
            Operand::Constant(ref c) => {
                match c.literal {
                    Literal::Item { def_id, ref substs } => {
                        let ty = self.tcx.lookup_item_type(def_id).ty;
                        if ty.is_fn() {
                            assert!(def_id.is_local());
                            let sig = ty.fn_sig().skip_binder();

                            let mut fn_did = def_id;
                            let fn_name = self.tcx.item_path_str(fn_did);
                            let fn_sig;
                            let mut call_kind = BinaryenCallKind::Direct;

                            if fn_name == "wasm::::print_i32" || fn_name == "wasm::::_print_i32" {
                                // extern wasm functions
                                fn_sig = sig.clone();
                                call_kind = BinaryenCallKind::Import;
                                self.import_wasm_extern(fn_did, sig);
                            } else {
                                let nid = self.tcx.map.as_local_node_id(fn_did).expect("");
                                let (nid, substs, sig) = if self.mir_map.map.contains_key(&nid) {
                                    (nid, *substs, sig)
                                } else {
                                    // only trait methods can have a Self parameter
                                    if substs.self_ty().is_none() {
                                        panic!("unimplemented fn trans: {:?}", fn_did);
                                    }

                                    let (resolved_def_id, resolved_substs) = traits::resolve_trait_method(self.tcx, fn_did, substs);
                                    let nid = self.tcx.map.as_local_node_id(resolved_def_id).expect("");
                                    let ty = self.tcx.lookup_item_type(resolved_def_id).ty;
                                    let sig = ty.fn_sig().skip_binder();

                                    fn_did = resolved_def_id;
                                    (nid, resolved_substs, sig)
                                };

                                let mir = &self.mir_map.map[&nid];

                                fn_sig = monomorphize::apply_param_substs(self.tcx, substs, sig);
                                {
                                    let mut ctxt = BinaryenFnCtxt {
                                        tcx: self.tcx,
                                        mir_map: self.mir_map,
                                        mir: mir,
                                        did: fn_did,
                                        sig: &fn_sig,
                                        module: self.module,
                                        fun_types: &mut self.fun_types,
                                        fun_names: &mut self.fun_names,
                                        fun_refs: &mut self.fun_refs,
                                        c_strings: &mut self.c_strings,
                                    };

                                    ctxt.trans();
                                }
                            }

                            let ret_ty = match fn_sig.output {
                                ty::FnOutput::FnConverging(ref t) => {
                                    if !t.is_nil() {
                                        rust_ty_to_binaryen(t)
                                    } else {
                                        BinaryenNone()
                                    }
                                }
                                _ => panic!()
                            };

                            Some((self.fun_names[&(fn_did, fn_sig)].as_ptr(), ret_ty, call_kind))
                        } else {
                            panic!();
                        }
                    }
                    _ => panic!("{:?}", c)
                }
            }
            _ => panic!()
        }
    }

    fn import_wasm_extern(&mut self, did: DefId, sig: &ty::FnSig<'tcx>) {
        if self.fun_names.contains_key(&(did, sig.clone())) {
            return;
        }

        // import print i32
        let print_i32_name = CString::new("print_i32").expect("");
        let print_i32 = print_i32_name.as_ptr();
        self.fun_names.insert((did, sig.clone()), print_i32_name.clone());
        self.c_strings.push(print_i32_name);

        let spectest_module_name = CString::new("spectest").expect("");
        let spectest_module = spectest_module_name.as_ptr();
        self.c_strings.push(spectest_module_name);

        let print_fn_name = CString::new("print").expect("");
        let print_fn = print_fn_name.as_ptr();
        self.c_strings.push(print_fn_name);

        let print_i32_args = [BinaryenInt32()];
        unsafe {
            let print_i32_ty = BinaryenAddFunctionType(self.module,
                                                       print_i32,
                                                       BinaryenNone(),
                                                       print_i32_args.as_ptr(),
                                                       BinaryenIndex(1));
            BinaryenAddImport(self.module,
                              print_i32,
                              spectest_module,
                              print_fn,
                              print_i32_ty);
        }
    }
}

fn rust_ty_to_binaryen<'tcx>(t: Ty<'tcx>) -> BinaryenType {
    // FIXME zero-sized-types
    match t.sty {
        ty::TyFloat(FloatTy::F32) => {
            BinaryenFloat32()
        },
        ty::TyFloat(FloatTy::F64) => {
            BinaryenFloat64()
        },
        ty::TyInt(IntTy::I64) | ty::TyUint(UintTy::U64) => {
            BinaryenInt64()
        }
        _ => {
            BinaryenInt32()
        }
    }
}

fn sanitize_symbol(s: &str) -> String {
    s.chars().map(|c| {
        match c {
            '<' | '>' | ' ' => '_',
            _ => c
        }
    }).collect()
}