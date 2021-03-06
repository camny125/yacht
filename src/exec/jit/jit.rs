use crate::{
    exec::{
        instruction::*,
        jit::{builtin::*, cfg::*},
    },
    metadata::{assembly::*, class::*, image::*, metadata::*, method::*, signature::*, token::*},
    util::{name_path::*, resolver::*},
};
use id_arena;
use llvm;
use llvm::{core::*, prelude::*};
use rustc_hash::FxHashMap;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::CString;
use std::ptr;

thread_local! {
    pub static STRING_METHOD_TABLE_PTR: RefCell<Option<MethodTablePtrTy>> = {
        RefCell::new(None)
    }
}

macro_rules! cstr0 {
    () => {
        CString::new("").unwrap().as_ptr()
    };
}

fn alloc_raw_method_table(len: usize) -> MethodTablePtrTy {
    Box::into_raw(vec![0 as *mut ::std::ffi::c_void; len].into_boxed_slice()) as MethodTablePtrTy
}

type TypeId = id_arena::Id<Type>;

pub type MethodTablePtrTy = *mut *mut ::std::ffi::c_void;

pub type CResult<T> = Result<T, Error>;

#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    CouldntCompile,
    General,
}

#[derive(Debug, Clone)]
pub enum BasicBlockInfo {
    Positioned(LLVMBasicBlockRef),
    Unpositioned(LLVMBasicBlockRef),
}

#[derive(Debug, Clone, Copy)]
pub struct TypedValue {
    pub ty: id_arena::Id<Type>,
    pub val: LLVMValueRef,
}

#[derive(Debug, Clone)]
pub struct PhiStack {
    src_bb: LLVMBasicBlockRef,
    stack: Vec<TypedValue>,
}

#[derive(Clone)]
pub struct SharedEnvironment {
    /// Builtin methods and methods belonging to the other assemblies. Searchable with ``MethodPath``.
    pub methods: BuiltinFunctions, // ``BuiltinFunction`` is bad name. Should be like 'MethodMap'

    /// All classes. Searchable with ``TypePath``.
    pub class_types: ClassTypesNameResolver,

    /// All method tables. Searchable with their pointers.
    pub method_table_map: FxHashMap<MethodTablePtrTy, (LLVMValueRef, Vec<LLVMValueRef>)>,

    /// LLVM Context
    pub context: LLVMContextRef,

    /// LLVM Module
    pub module: LLVMModuleRef,

    /// LLVM Builder for globally use
    pub builder: LLVMBuilderRef,

    /// LLVM Pass Manager
    pub pass_mgr: LLVMPassManagerRef,

    /// Id arena for ``Type``
    pub ty_arena: id_arena::Arena<Type>,
}

#[derive(Clone)]
pub struct AssemblyUniqueEnvironment {
    pub generated: FxHashMap<RVA, LLVMValueRef>,
    pub compile_queue: VecDeque<(LLVMValueRef, MethodInfoRef)>,
}

pub struct JITCompiler<'a> {
    pub assembly: &'a mut Assembly,
    pub shared_env: &'a mut SharedEnvironment,
    pub asm_env: AssemblyUniqueEnvironment,
    pub env: CodeEnvironment,
    pub generating: Option<LLVMValueRef>,
    pub basic_blocks: FxHashMap<usize, BasicBlockInfo>,
    pub phi_stack: FxHashMap<usize, Vec<PhiStack>>, // destination,
}

#[derive(Debug, Clone)]
pub struct CodeEnvironment {
    pub arguments: FxHashMap<usize, TypedValue>,
    pub locals: FxHashMap<usize, TypedValue>,
}

#[derive(Debug, Clone)]
pub struct ClassTypesNameResolver {
    base: NameResolver<(LLVMTypeRef, MethodTablePtrTy)>,
}

impl<'a> JITCompiler<'a> {
    pub unsafe fn new(assembly: &'a mut Assembly, shared_env: &'a mut SharedEnvironment) -> Self {
        llvm::target::LLVM_InitializeNativeTarget();
        llvm::target::LLVM_InitializeNativeAsmPrinter();
        llvm::target::LLVM_InitializeNativeAsmParser();
        llvm::target::LLVM_InitializeAllTargetMCs();
        llvm::execution_engine::LLVMLinkInMCJIT();

        let mut self_ = Self {
            assembly,
            shared_env,
            env: CodeEnvironment::new(),
            asm_env: AssemblyUniqueEnvironment::new(),
            generating: None,
            basic_blocks: FxHashMap::default(),
            phi_stack: FxHashMap::default(),
        };

        self_.setup_mscorlib_system();

        self_
    }

    pub unsafe fn new_without_mscorlib_init(
        assembly: &'a mut Assembly,
        shared_env: &'a mut SharedEnvironment,
    ) -> Self {
        Self {
            assembly,
            shared_env,
            env: CodeEnvironment::new(),
            asm_env: AssemblyUniqueEnvironment::new(),
            generating: None,
            basic_blocks: FxHashMap::default(),
            phi_stack: FxHashMap::default(),
        }
    }

    pub fn with_asm_env(mut self, asm_env: AssemblyUniqueEnvironment) -> Self {
        self.asm_env = asm_env;
        self
    }

    // TODO: Should take arguments for ``method``
    pub unsafe fn run_method(&mut self, method: LLVMValueRef) {
        let mut ee = 0 as llvm::execution_engine::LLVMExecutionEngineRef;
        let mut error = 0 as *mut i8;
        if llvm::execution_engine::LLVMCreateExecutionEngineForModule(
            &mut ee,
            self.shared_env.module,
            &mut error,
        ) != 0
        {
            panic!("llvm error: failed to initialize execute engine")
        }

        for f in self.shared_env.methods.list_all_function() {
            if f.function as usize != 0 {
                llvm::execution_engine::LLVMAddGlobalMapping(ee, f.llvm_function, f.function);
            }
        }

        llvm::execution_engine::LLVMRunFunction(ee, method, 0, vec![].as_mut_ptr());
    }

    pub unsafe fn generate_method_as_main(&mut self, method_ref: &MethodInfoRef) -> LLVMValueRef {
        self.generate_all_class_and_method();

        self.basic_blocks.clear();
        self.phi_stack.clear();
        self.env = CodeEnvironment::new();

        let method_info = method_ref.borrow();
        let method = method_info.as_mdef();

        let basic_blocks = CFGMaker::new().make_basic_blocks(&method.body);
        let (ret_ty, mut params_ty): (LLVMTypeRef, Vec<LLVMTypeRef>) =
            { (Type::new(ElementType::Void).to_llvmty(self), vec![]) };
        let func_ty = LLVMFunctionType(ret_ty, params_ty.as_mut_ptr(), params_ty.len() as u32, 0);
        let func = LLVMAddFunction(
            self.shared_env.module,
            CString::new("yacht-Main").unwrap().as_ptr(),
            func_ty,
        );

        self.generating = Some(func);

        let bb_before_entry = LLVMAppendBasicBlockInContext(
            self.shared_env.context,
            func,
            CString::new("initialize").unwrap().as_ptr(),
        );

        let bb_entry = LLVMAppendBasicBlockInContext(
            self.shared_env.context,
            func,
            CString::new("entry").unwrap().as_ptr(),
        );

        self.basic_blocks
            .insert(0, BasicBlockInfo::Unpositioned(bb_entry));

        // Declare locals
        for (i, ty) in method.locals_ty.iter().enumerate() {
            self.get_local(i, Some(&ty));
        }

        for block in &basic_blocks {
            // Exclude entry block (whose .start == 0) since it's already inserted
            if block.start > 0 {
                self.basic_blocks.insert(
                    block.start,
                    BasicBlockInfo::Unpositioned(LLVMAppendBasicBlock(func, cstr0!())),
                );
            }
        }

        // Compile all the basic blocks

        for i in 0..basic_blocks.len() {
            self.compile_block(&basic_blocks, i, &vec![]).unwrap();
        }

        self.generate_queued_methods();

        // Set all the class methods to the appropriate method_table

        LLVMPositionBuilderAtEnd(self.shared_env.builder, bb_before_entry);

        for (_, (llvm_method_table, methods)) in &self.shared_env.method_table_map {
            if methods.len() == 0 {
                continue;
            }

            let method_table = self.typecast(
                *llvm_method_table,
                LLVMPointerType(LLVMTypeOf(methods[0]), 0),
            );

            for (i, vmethod) in methods.iter().enumerate() {
                self.store2element(method_table, vec![self.llvm_int32(i as u64)], *vmethod);
            }
        }

        LLVMBuildBr(self.shared_env.builder, bb_entry);

        // Append ``ret void`` to the incomplete basic blocks

        let mut iter_bb = LLVMGetFirstBasicBlock(func);

        while iter_bb != ptr::null_mut() {
            if LLVMIsATerminatorInst(LLVMGetLastInstruction(iter_bb)) == ptr::null_mut() {
                let terminator_builder = LLVMCreateBuilderInContext(self.shared_env.context);
                LLVMPositionBuilderAtEnd(terminator_builder, iter_bb);
                LLVMBuildRetVoid(terminator_builder);
            }
            iter_bb = LLVMGetNextBasicBlock(iter_bb);
        }

        when_debug!(LLVMDumpModule(self.shared_env.module));

        llvm::analysis::LLVMVerifyFunction(
            func,
            llvm::analysis::LLVMVerifierFailureAction::LLVMAbortProcessAction,
        );

        LLVMRunPassManager(self.shared_env.pass_mgr, self.shared_env.module);

        func
    }

    unsafe fn generate_func(&mut self, func: LLVMValueRef, method_ref: &MethodInfoRef) {
        self.generating = Some(func);
        self.env = CodeEnvironment::new();

        let method_info = method_ref.borrow();
        let method = method_info.as_mdef();
        let method_ty = method.ty.as_fnptr().unwrap();
        let basic_blocks = CFGMaker::new().make_basic_blocks(&method.body);
        let ret_ty = LLVMGetElementType(LLVMGetReturnType(LLVMTypeOf(func)));
        let bb_entry = LLVMAppendBasicBlockInContext(
            self.shared_env.context,
            func,
            CString::new("entry").unwrap().as_ptr(),
        );

        self.basic_blocks
            .insert(0, BasicBlockInfo::Unpositioned(bb_entry));
        LLVMPositionBuilderAtEnd(self.shared_env.builder, bb_entry);

        let shift = if method_ty.has_this() {
            LLVMBuildStore(
                self.shared_env.builder,
                LLVMGetParam(func, 0),
                self.get_argument(
                    0,
                    Some(&Type::new(ElementType::Class(method.class.clone()))),
                ),
            );
            1
        } else {
            0
        };

        for (i, ty) in method_ty.params.iter().enumerate() {
            LLVMBuildStore(
                self.shared_env.builder,
                LLVMGetParam(func, (i + shift) as u32),
                self.get_argument(i + shift, Some(&ty)),
            );
        }

        // Declare locals
        for (i, ty) in method.locals_ty.iter().enumerate() {
            self.get_local(i, Some(&ty));
        }

        for block in &basic_blocks {
            if block.start > 0 {
                // Exclude entry block (whose .start == 0) since it's already inserted
                self.basic_blocks.insert(
                    block.start,
                    BasicBlockInfo::Unpositioned(LLVMAppendBasicBlock(func, cstr0!())),
                );
            }
        }

        for i in 0..basic_blocks.len() {
            self.compile_block(&basic_blocks, i, &vec![]).unwrap();
        }

        let last_block = basic_blocks.last().unwrap();
        let bb_last = (*self.basic_blocks.get(&last_block.start).unwrap()).retrieve();
        LLVMPositionBuilderAtEnd(self.shared_env.builder, bb_last);
        if cur_bb_has_no_terminator(self.shared_env.builder) {
            if LLVMGetTypeKind(ret_ty) == llvm::LLVMTypeKind::LLVMVoidTypeKind {
                LLVMBuildRetVoid(self.shared_env.builder);
            } else {
                LLVMBuildRet(self.shared_env.builder, LLVMConstNull(ret_ty));
            }
        }

        let mut iter_bb = LLVMGetFirstBasicBlock(func);
        while iter_bb != ptr::null_mut() {
            if LLVMIsATerminatorInst(LLVMGetLastInstruction(iter_bb)) == ptr::null_mut() {
                let terminator_builder = LLVMCreateBuilderInContext(self.shared_env.context);
                LLVMPositionBuilderAtEnd(terminator_builder, iter_bb);
                if LLVMGetTypeKind(ret_ty) == llvm::LLVMTypeKind::LLVMVoidTypeKind {
                    LLVMBuildRetVoid(terminator_builder);
                } else {
                    LLVMBuildRet(terminator_builder, LLVMConstNull(ret_ty));
                }
            }
            iter_bb = LLVMGetNextBasicBlock(iter_bb);
        }

        self.basic_blocks.clear();
        self.phi_stack.clear();
    }

    unsafe fn setup_mscorlib_system(&mut self) {
        for class_ref in &[
            mscorlib_system_object(),
            mscorlib_system_int32(),
            mscorlib_system_string(),
        ] {
            let class = class_ref.borrow();
            self.get_llvm_class_type(&class);
            let (method_table_ptr, _) = self.ensure_all_class_methods_compiled(&class);
            if class.name == "String" {
                STRING_METHOD_TABLE_PTR.with(|smp| *smp.borrow_mut() = Some(method_table_ptr));
            }
        }
    }

    pub unsafe fn define_all_class(&mut self) {
        let classes = self
            .assembly
            .image
            .class_cache
            .iter()
            .filter_map(|(_tok, class)| {
                let class = class.borrow();
                // ``class_cache`` may contain classes belonging to another assembly. Here exclude
                // them.
                match class.resolution_scope {
                    // TODO: Support all possible ResolutionScope
                    ResolutionScope::AssemblyRef { ref name } if name == &self.assembly.name => {
                        Some(class.clone())
                    }
                    _ => None,
                }
            })
            .collect::<Vec<ClassInfo>>();

        for class in classes {
            self.get_llvm_class_type(&class); // define llvm class(structure)
            self.ensure_all_class_methods_compiled(&class);
        }
    }

    pub unsafe fn define_all_method(&mut self) {
        let methods = self
            .assembly
            .image
            .method_cache
            .iter()
            .map(|(_, minforef)| minforef.borrow().clone().into_mdef())
            .collect::<Vec<MethodDefInfo>>();

        for m in methods {
            let llvm_function = self.get_function_by_rva(m.rva);
            let class = m.class.borrow();
            let method_path = ((&*class).into(): TypePath).with_method_name(m.name.as_str());
            self.shared_env.methods.map.add(
                method_path,
                vec![Function {
                    llvm_function,
                    function: 0 as *mut ::std::ffi::c_void,
                    ty: m.ty,
                }],
            )
        }
    }

    pub unsafe fn generate_all_class_and_method(&mut self) {
        let mut asm_envs = vec![];
        let mut asms = FxHashMap::default();

        self.assembly
            .image
            .collect_all_reachable_assemblies(&mut asms);

        for (_name, asmref) in &asms {
            let mut asm = asmref.borrow_mut();
            let mut compiler =
                JITCompiler::new_without_mscorlib_init(&mut *asm, &mut self.shared_env);
            compiler.define_all_class();
            compiler.define_all_method();
            asm_envs.push(compiler.asm_env);
        }

        for (asm_env, (_name, asmref)) in asm_envs.into_iter().zip(asms.iter()) {
            let mut asmref = asmref.borrow_mut();
            JITCompiler::new_without_mscorlib_init(&mut *asmref, &mut self.shared_env)
                .with_asm_env(asm_env)
                .generate_queued_methods();
        }
    }

    pub unsafe fn generate_queued_methods(&mut self) {
        while let Some((func, method)) = self.asm_env.compile_queue.pop_front() {
            self.generate_func(func, &method);
        }
    }

    unsafe fn get_local_ty(&mut self, id: usize) -> TypeId {
        self.env.locals.get(&id).unwrap().ty
    }

    unsafe fn get_argument_ty(&mut self, id: usize) -> TypeId {
        self.env.arguments.get(&id).unwrap().ty
    }

    unsafe fn get_local(&mut self, id: usize, ty: Option<&Type>) -> LLVMValueRef {
        if let Some(v) = self.env.locals.get(&id) {
            return v.val;
        }

        let func = self.generating.unwrap();
        let builder = LLVMCreateBuilderInContext(self.shared_env.context);
        let entry_bb = LLVMGetEntryBasicBlock(func);
        let first_inst = LLVMGetFirstInstruction(entry_bb);

        // A variable must be declared at the first point of entry block
        if first_inst == ptr::null_mut() {
            LLVMPositionBuilderAtEnd(builder, entry_bb);
        } else {
            LLVMPositionBuilderBefore(builder, first_inst);
        }

        let var = LLVMBuildAlloca(builder, ty.unwrap().to_llvmty(self), cstr0!());
        let ty_id = self.shared_env.type_id(ty.unwrap());

        self.env.locals.insert(id, TypedValue::new(ty_id, var));

        var
    }

    unsafe fn get_argument(&mut self, id: usize, ty: Option<&Type>) -> LLVMValueRef {
        if let Some(v) = self.env.arguments.get(&id) {
            return v.val;
        }

        let func = self.generating.unwrap();
        let builder = LLVMCreateBuilderInContext(self.shared_env.context);
        let entry_bb = LLVMGetEntryBasicBlock(func);
        let first_inst = LLVMGetFirstInstruction(entry_bb);

        // A variable is always declared at the first point of entry block
        if first_inst == ptr::null_mut() {
            LLVMPositionBuilderAtEnd(builder, entry_bb);
        } else {
            LLVMPositionBuilderBefore(builder, first_inst);
        }

        let var = LLVMBuildAlloca(builder, ty.unwrap().to_llvmty(self), cstr0!());
        let ty_id = self.shared_env.type_id(ty.unwrap());

        self.env.arguments.insert(id, TypedValue::new(ty_id, var));

        var
    }

    // Returns destination
    unsafe fn compile_block(
        &mut self,
        blocks: &[BasicBlock],
        idx: usize,
        init_stack: &Vec<TypedValue>,
    ) -> CResult<usize> {
        #[rustfmt::skip]
        macro_rules! cur_block { () => {{ &blocks[idx] }}; };

        fn find_block(start: usize, blocks: &[BasicBlock]) -> Option<usize> {
            blocks
                .iter()
                .enumerate()
                .find(|(_, block)| block.start == start)
                .map_or(None, |(i, _)| Some(i))
        }

        let bb = self.basic_blocks.get_mut(&cur_block!().start).unwrap();

        if bb.is_positioned() {
            // this block is already generated
            return Ok(0);
        }

        LLVMPositionBuilderAtEnd(self.shared_env.builder, bb.set_positioned().retrieve());

        let phi_stack = self.build_phi_stack(cur_block!().start, init_stack);
        let stack = self.compile_bytecode(cur_block!(), phi_stack)?;

        match &cur_block!().kind {
            BrKind::ConditionalJmp { destinations } => {
                let mut d = 0;
                for dst in destinations {
                    let i = find_block(*dst, blocks).unwrap();
                    d = self.compile_block(blocks, i, &stack)?;
                    // TODO: All ``d`` must be the same
                }
                Ok(d)
            }
            BrKind::UnconditionalJmp { destination } => {
                let src_bb = self.get_basic_block(cur_block!().start).retrieve();
                self.phi_stack
                    .entry(*destination)
                    .or_insert(vec![])
                    .push(PhiStack { src_bb, stack });
                Ok(*destination)
            }
            BrKind::ImplicitJmp { destination } => {
                let src_bb = self.get_basic_block(cur_block!().start).retrieve();
                self.phi_stack
                    .entry(*destination)
                    .or_insert(vec![])
                    .push(PhiStack { src_bb, stack });
                if cur_bb_has_no_terminator(self.shared_env.builder) {
                    let bb = self.get_basic_block(*destination).retrieve();
                    LLVMBuildBr(self.shared_env.builder, bb);
                }
                Ok(*destination)
            }
            _ => Ok(0),
        }
    }

    unsafe fn build_phi_stack(
        &mut self,
        start: usize,
        init_stack: &Vec<TypedValue>,
    ) -> Vec<TypedValue> {
        let mut stack = init_stack.clone();

        if let Some(phi_stacks) = self.phi_stack.get(&start) {
            let init_stack_size = stack.len();

            // Firstly, build llvm phi which needs a type of all conceivable values.
            let src_bb = phi_stacks[0].src_bb;
            for TypedValue { val, ty } in &phi_stacks[0].stack {
                let phi = LLVMBuildPhi(self.shared_env.builder, LLVMTypeOf(*val), cstr0!());
                LLVMAddIncoming(phi, vec![*val].as_mut_ptr(), vec![src_bb].as_mut_ptr(), 1);
                stack.push(TypedValue::new(*ty, phi));
            }

            for phi_stack in &phi_stacks[1..] {
                let src_bb = phi_stack.src_bb;
                for (i, TypedValue { val, .. }) in (&phi_stack.stack).iter().enumerate() {
                    let phi = stack[init_stack_size + i].val;
                    LLVMAddIncoming(phi, vec![*val].as_mut_ptr(), vec![src_bb].as_mut_ptr(), 1);
                }
            }
        }

        stack
    }

    unsafe fn compile_bytecode(
        &mut self,
        block: &BasicBlock,
        mut stack: Vec<TypedValue>,
    ) -> CResult<Vec<TypedValue>> {
        #[rustfmt::skip]
        macro_rules! binop { ($iop:ident, $fop:ident) => {{
            let val2 = stack.pop().unwrap();
            let val1 = stack.pop().unwrap();
            let ty = &self.shared_env.ty_arena[val1.ty];
            stack.push(if ty.is_int() {
                TypedValue::new(val1.ty, concat_idents!(LLVMBuild, $iop)(self.shared_env.builder,
                                val1.val, self.typecast(val2.val, LLVMTypeOf(val1.val)), cstr0!()))
            } else if ty.is_float() {
                TypedValue::new(val1.ty, concat_idents!(LLVMBuild, $fop)(self.shared_env.builder,
                                val1.val, val2.val, cstr0!()))
            } else { unimplemented!("{:?}", ty) })
        }}}
        #[rustfmt::skip]
        macro_rules! unaryop { ($iop:ident, $fop:ident) => {{
            let val = stack.pop().unwrap();
            let ty = &self.shared_env.ty_arena[val.ty];
            stack.push(if ty.is_int() {
                TypedValue::new(val.ty, concat_idents!(LLVMBuild, $iop)(self.shared_env.builder, val.val, cstr0!()))
            } else if ty.is_float() {
                TypedValue::new(val.ty, concat_idents!(LLVMBuild, $fop)(self.shared_env.builder, val.val, cstr0!()))
            } else { unimplemented!("{:?}", ty) });
        }}}
        #[rustfmt::skip]
        macro_rules! push_i4 { ($n:expr) => {
             stack.push(TypedValue::new(
                self.shared_env.type_id(&Type::i4_ty()), self.llvm_int32($n as u64),
            ))
        }}
        #[rustfmt::skip]
        macro_rules! push_r8 { ($n:expr) => {
             stack.push(TypedValue::new(
                self.shared_env.type_id(&Type::r8_ty()), self.llvm_real($n),
            ))
        }}
        #[rustfmt::skip]
        macro_rules! ldloc { ($n:expr) => {
            stack.push(TypedValue::new(
                self.get_local_ty($n),
                LLVMBuildLoad(self.shared_env.builder, self.get_local($n, None), cstr0!()),
            ))
        }}
        #[rustfmt::skip]
        macro_rules! stloc { ($n:expr) => {{
            let val = self.get_local($n, None);
            LLVMBuildStore(self.shared_env.builder,
                self.typecast(stack.pop().unwrap().val,
                    LLVMGetElementType(LLVMTypeOf(val))), val);
        }}; }
        #[rustfmt::skip]
        macro_rules! ldarg { ($n:expr) => {
            stack.push(TypedValue::new(
                self.get_argument_ty($n),
                LLVMBuildLoad(self.shared_env.builder, self.get_argument($n, None), cstr0!()),
            ))
        }}
        #[rustfmt::skip]
        macro_rules! starg { ($n:expr) => {{
            let val = self.get_argument($n, None);
            LLVMBuildStore(self.shared_env.builder,
                self.typecast(stack.pop().unwrap().val,
                    LLVMGetElementType(LLVMTypeOf(val))), val);
        }}; }
        #[rustfmt::skip]
        macro_rules! conv { ($ty:expr) => {{
            let val = stack.pop().unwrap().val;
            stack.push(TypedValue::new(
                self.shared_env.type_id(&$ty),
                {
                    let llvm_ty = $ty.to_llvmty(self);
                    self.typecast(val, llvm_ty)
                }
            ))
        }}; }

        let code = &block.code;

        for instr in code {
            match instr {
                Instruction::Ldnull => push_i4!(0),
                Instruction::Ldstr(us_offset) => self.create_new_string(
                    &mut stack,
                    self.assembly.image.get_user_string(*us_offset).clone(),
                ),
                Instruction::Ldc_I4_M1 => push_i4!(0 - 1),
                Instruction::Ldc_I4_0 => push_i4!(0),
                Instruction::Ldc_I4_1 => push_i4!(1),
                Instruction::Ldc_I4_2 => push_i4!(2),
                Instruction::Ldc_I4_3 => push_i4!(3),
                Instruction::Ldc_I4_4 => push_i4!(4),
                Instruction::Ldc_I4_5 => push_i4!(5),
                Instruction::Ldc_I4_6 => push_i4!(6),
                Instruction::Ldc_I4_7 => push_i4!(7),
                Instruction::Ldc_I4_8 => push_i4!(8),
                Instruction::Ldc_I4_S(n) => push_i4!(*n),
                Instruction::Ldc_I4(n) => push_i4!(*n),
                Instruction::Ldc_R8(f) => push_r8!(*f),
                Instruction::Ldloc_0 => ldloc!(0),
                Instruction::Ldloc_1 => ldloc!(1),
                Instruction::Ldloc_2 => ldloc!(2),
                Instruction::Ldloc_3 => ldloc!(3),
                Instruction::Ldloc_S(n) => ldloc!(*n as usize),
                Instruction::Ldfld(token) => self.gen_instr_ldfld(&mut stack, *token),
                Instruction::Ldelem_U1 => self.gen_instr_ldelem_i1(&mut stack),
                Instruction::Ldelem_I1 => self.gen_instr_ldelem_i1(&mut stack),
                Instruction::Ldelem_I4 => self.gen_instr_ldelem_i4(&mut stack),
                Instruction::Ldelem_ref => self.gen_instr_ldelem_ref(&mut stack),
                Instruction::Stloc_0 => stloc!(0),
                Instruction::Stloc_1 => stloc!(1),
                Instruction::Stloc_2 => stloc!(2),
                Instruction::Stloc_3 => stloc!(3),
                Instruction::Stloc_S(n) => stloc!(*n as usize),
                Instruction::Stfld(token) => self.gen_instr_stfld(&mut stack, *token),
                Instruction::Stelem_I1 => self.gen_instr_stelem_i1(&mut stack),
                Instruction::Stelem_I4 => self.gen_instr_stelem_i4(&mut stack),
                Instruction::Stelem_ref => self.gen_instr_stelem_ref(&mut stack),
                Instruction::Starg_S(n) => starg!(*n as usize),
                Instruction::Ldarg_0 => ldarg!(0),
                Instruction::Ldarg_1 => ldarg!(1),
                Instruction::Ldarg_2 => ldarg!(2),
                Instruction::Ldarg_3 => ldarg!(3),
                Instruction::Ldarg_S(n) => ldarg!(*n as usize),
                Instruction::Ldlen => self.gen_instr_ldlen(&mut stack),
                Instruction::Conv_I4 => conv!(Type::i4_ty()),
                Instruction::Conv_I8 => conv!(Type::i8_ty()),
                Instruction::Conv_R8 => conv!(Type::r8_ty()),
                Instruction::Conv_R_un => self.gen_instr_conv_r_un(&mut stack),
                Instruction::Pop => {
                    stack.pop();
                }
                Instruction::Dup => {
                    stack.push(*stack.last().unwrap());
                }
                Instruction::Call(token) => self.gen_instr_call(&mut stack, *token),
                Instruction::CallVirt(token) => self.gen_instr_callvirt(&mut stack, *token),
                Instruction::Box(token) => self.gen_instr_box(&mut stack, *token),
                Instruction::Newobj(token) => self.gen_instr_newobj(&mut stack, *token),
                Instruction::Newarr(token) => self.gen_instr_newarr(&mut stack, *token),
                Instruction::Add => binop!(Add, FAdd),
                Instruction::Sub => binop!(Sub, FSub),
                Instruction::Mul => binop!(Mul, FMul),
                Instruction::Div => binop!(SDiv, FDiv),
                Instruction::Rem => binop!(SRem, FRem),
                Instruction::Rem_un => binop!(URem, FRem),
                Instruction::Xor => binop!(Xor, Xor),
                Instruction::Shl => binop!(Shl, Shl),
                Instruction::Shr => binop!(AShr, AShr),
                Instruction::Shr_un => binop!(LShr, LShr),
                Instruction::Neg => unaryop!(Neg, FNeg),
                Instruction::Ret => {
                    let ret_ty =
                        LLVMGetElementType(LLVMGetReturnType(LLVMTypeOf(self.generating.unwrap())));
                    if LLVMGetTypeKind(ret_ty) == llvm::LLVMTypeKind::LLVMVoidTypeKind {
                        LLVMBuildRetVoid(self.shared_env.builder);
                    } else {
                        let val = stack.pop().unwrap().val;
                        LLVMBuildRet(self.shared_env.builder, self.typecast(val, ret_ty));
                    }
                }
                Instruction::Brfalse { .. } | Instruction::Brtrue { .. } => {
                    let val1 = stack.pop().unwrap();
                    let cond_val = LLVMBuildICmp(
                        self.shared_env.builder,
                        match instr {
                            Instruction::Brfalse { .. } => llvm::LLVMIntPredicate::LLVMIntEQ,
                            Instruction::Brtrue { .. } => llvm::LLVMIntPredicate::LLVMIntNE,
                            _ => unreachable!(),
                        },
                        val1.val,
                        LLVMConstNull(LLVMTypeOf(val1.val)),
                        cstr0!(),
                    );
                    let destinations = block.kind.get_conditional_jump_destinations();
                    let bb_then = self.get_basic_block(destinations[0]).retrieve();
                    let bb_else = self.get_basic_block(destinations[1]).retrieve();
                    LLVMBuildCondBr(self.shared_env.builder, cond_val, bb_then, bb_else);
                }
                Instruction::Bge { .. }
                | Instruction::Bge_un { .. }
                | Instruction::Blt { .. }
                | Instruction::Ble { .. }
                | Instruction::Ble_un { .. }
                | Instruction::Beq { .. }
                | Instruction::Bne_un { .. }
                | Instruction::Bgt { .. } => {
                    let val2 = stack.pop().unwrap();
                    let val1 = stack.pop().unwrap();
                    let ty = &self.shared_env.ty_arena[val1.ty];
                    let cond_val = if ty.is_int() {
                        LLVMBuildICmp(
                            self.shared_env.builder,
                            match instr {
                                Instruction::Bge { .. } => llvm::LLVMIntPredicate::LLVMIntSGE,
                                Instruction::Bge_un { .. } => llvm::LLVMIntPredicate::LLVMIntUGE,
                                Instruction::Blt { .. } => llvm::LLVMIntPredicate::LLVMIntSLT,
                                Instruction::Ble { .. } => llvm::LLVMIntPredicate::LLVMIntSLE,
                                Instruction::Ble_un { .. } => llvm::LLVMIntPredicate::LLVMIntULE,
                                Instruction::Bgt { .. } => llvm::LLVMIntPredicate::LLVMIntSGT,
                                Instruction::Beq { .. } => llvm::LLVMIntPredicate::LLVMIntEQ,
                                Instruction::Bne_un { .. } => llvm::LLVMIntPredicate::LLVMIntNE,
                                _ => unreachable!(),
                            },
                            val1.val,
                            self.typecast(val2.val, LLVMTypeOf(val1.val)),
                            cstr0!(),
                        )
                    } else if ty.is_float() {
                        LLVMBuildFCmp(
                            self.shared_env.builder,
                            match instr {
                                Instruction::Bge { .. } => llvm::LLVMRealPredicate::LLVMRealOGE,
                                Instruction::Bge_un { .. } => llvm::LLVMRealPredicate::LLVMRealOGE,
                                Instruction::Blt { .. } => llvm::LLVMRealPredicate::LLVMRealOLT,
                                Instruction::Ble { .. } => llvm::LLVMRealPredicate::LLVMRealOLE,
                                Instruction::Ble_un { .. } => llvm::LLVMRealPredicate::LLVMRealOLE,
                                Instruction::Bgt { .. } => llvm::LLVMRealPredicate::LLVMRealOGT,
                                Instruction::Beq { .. } => llvm::LLVMRealPredicate::LLVMRealOEQ,
                                Instruction::Bne_un { .. } => llvm::LLVMRealPredicate::LLVMRealONE,
                                _ => unreachable!(),
                            },
                            val1.val,
                            val2.val,
                            cstr0!(),
                        )
                    } else {
                        unimplemented!("{:?}", ty)
                    };
                    let destinations = block.kind.get_conditional_jump_destinations();
                    let bb_then = self.get_basic_block(destinations[0]).retrieve();
                    let bb_else = self.get_basic_block(destinations[1]).retrieve();
                    LLVMBuildCondBr(self.shared_env.builder, cond_val, bb_then, bb_else);
                }
                Instruction::Br { .. } => {
                    let destination = block.kind.get_unconditional_jump_destination();
                    let bb_br = self.get_basic_block(destination).retrieve();
                    if cur_bb_has_no_terminator(self.shared_env.builder) {
                        LLVMBuildBr(self.shared_env.builder, bb_br);
                    }
                }
                Instruction::Clt | Instruction::Cgt | Instruction::Ceq => {
                    let val2 = stack.pop().unwrap();
                    let val1 = stack.pop().unwrap();
                    let cond_val = self.typecast(
                        LLVMBuildICmp(
                            self.shared_env.builder,
                            match instr {
                                Instruction::Cgt { .. } => llvm::LLVMIntPredicate::LLVMIntSGT,
                                Instruction::Clt { .. } => llvm::LLVMIntPredicate::LLVMIntSLT,
                                Instruction::Ceq { .. } => llvm::LLVMIntPredicate::LLVMIntEQ,
                                _ => unreachable!(),
                            },
                            val1.val,
                            val2.val,
                            cstr0!(),
                        ),
                        LLVMInt32TypeInContext(self.shared_env.context),
                    );
                    stack.push(TypedValue::new(
                        self.shared_env.type_id(&Type::i4_ty()),
                        cond_val,
                    ));
                }
            }
        }

        Ok(stack)
    }

    unsafe fn create_new_string(&mut self, stack: &mut Vec<TypedValue>, s: Vec<u16>) {
        let class_system_string_ref = mscorlib_system_string();
        let class_system_string = class_system_string_ref.borrow();
        let class_string = self
            .shared_env
            .class_types
            .get(TypePath(vec!["mscorlib", "System", "String"]))
            .unwrap();
        let new_string = self.typecast(
            self.call_memory_alloc(self.get_size_of_llvm_class_type(class_string)),
            class_string,
        );
        let (_, method_table) = self.ensure_all_class_methods_compiled(&*class_system_string);
        self.store2element(
            new_string,
            vec![self.llvm_int32(0), self.llvm_int32(0)],
            method_table,
        );
        self.store2element(
            new_string,
            vec![self.llvm_int32(0), self.llvm_int32(1)],
            self.llvm_ptr(Box::into_raw(Box::new(s)) as *mut u8),
        );
        stack.push(TypedValue::new(
            self.shared_env.type_id(&Type::string_ty()),
            new_string,
        ));
    }

    unsafe fn gen_instr_call(&mut self, stack: &mut Vec<TypedValue>, token: Token) {
        self.gen_instr_general_call(stack, token, false)
    }

    unsafe fn gen_instr_callvirt(&mut self, stack: &mut Vec<TypedValue>, token: Token) {
        self.gen_instr_general_call(stack, token, true)
    }

    unsafe fn get_function_by_rva(&mut self, rva: u32) -> LLVMValueRef {
        if let Some(f) = self.asm_env.generated.get(&rva) {
            return *f;
        }

        let method_ref = self.assembly.image.get_method_by_rva(rva).unwrap();
        let method_info = method_ref.borrow();
        let method = method_info.as_mdef();
        let method_sig = method.ty.as_fnptr().unwrap();

        let ret_ty = method_sig.ret.to_llvmty(self);
        let mut params_ty = method_sig
            .params
            .iter()
            .map(|ty| ty.to_llvmty(self))
            .collect::<Vec<LLVMTypeRef>>();

        if method_sig.has_this() {
            params_ty.insert(0, self.get_llvm_class_type(&method.class.borrow()))
        }

        let func_ty = LLVMFunctionType(ret_ty, params_ty.as_mut_ptr(), params_ty.len() as u32, 0);
        let func = LLVMAddFunction(
            self.shared_env.module,
            CString::new(method.name.as_str()).unwrap().as_ptr(),
            func_ty,
        );

        self.asm_env.generated.insert(rva, func);
        self.asm_env
            .compile_queue
            .push_back((func, method_ref.clone()));

        func
    }

    unsafe fn gen_instr_general_call(
        &mut self,
        stack: &mut Vec<TypedValue>,
        token: Token,
        is_virtual: bool,
    ) {
        unsafe fn call(
            compiler: &mut JITCompiler,
            stack: &mut Vec<TypedValue>,
            func: LLVMValueRef,
            msig: &MethodSignature,
        ) {
            let (_, args) = get_arg_vals_from_stack(stack, msig.params.len(), msig.has_this());
            let ret = compiler.call_function(func, args);
            if !msig.ret.is_void() {
                stack.push(TypedValue::new(compiler.shared_env.type_id(&msig.ret), ret));
            }
        };

        unsafe fn callvirt(
            compiler: &mut JITCompiler,
            stack: &mut Vec<TypedValue>,
            method_idx: usize,
            method_sig: &MethodSignature,
            method_ty: LLVMTypeRef,
        ) {
            let (obj_ty_id, args) = get_arg_vals_from_stack(stack, method_sig.params.len(), true);
            let obj_llvm_ty = compiler.shared_env.ty_arena[obj_ty_id.unwrap()]
                .clone()
                .to_llvmty(compiler);
            let obj = compiler.typecast(args[0], obj_llvm_ty);

            let method_table =
                compiler.load_element(obj, vec![compiler.llvm_int32(0), compiler.llvm_int32(0)]);
            let raw_vmethod =
                compiler.load_element(method_table, vec![compiler.llvm_int32(method_idx as u64)]);
            let vmethod = compiler.typecast(raw_vmethod, method_ty);

            let ret = compiler.call_function(vmethod, args);
            if !method_sig.ret.is_void() {
                stack.push(TypedValue::new(
                    compiler.shared_env.type_id(&method_sig.ret),
                    ret,
                ));
            }
        };

        match self.assembly.image.metadata.get_table_entry(token).unwrap() {
            Table::MemberRef(mrt) => {
                let class_token = mrt.class2token();
                let class = &self
                    .assembly
                    .image
                    .metadata
                    .get_table_entry(class_token)
                    .unwrap();
                match class {
                    Table::TypeRef(trt) => {
                        let type_path = self.assembly.image.get_path_from_type_ref_table(trt);
                        let name = self.assembly.image.get_string(mrt.name);
                        let ty = self
                            .assembly
                            .image
                            .get_method_ref_type_from_signature(mrt.signature);
                        let f = match self
                            .shared_env
                            .methods
                            .get_method(type_path.with_method_name(&name), &ty)
                        {
                            Some(f) => f,
                            None => return,
                        };
                        let method_sig = ty.as_fnptr().unwrap();
                        if is_virtual {
                            let llvm_f_ty = LLVMTypeOf(f.llvm_function);
                            let midx =
                                (self.assembly.image.get_class(class_token).unwrap().borrow())
                                    .get_method_index(name)
                                    .unwrap();
                            callvirt(self, stack, midx, method_sig, llvm_f_ty);
                        } else {
                            let llvm_f = f.llvm_function;
                            call(self, stack, llvm_f, method_sig);
                        }
                    }
                    _ => unimplemented!(),
                }
            }
            Table::MethodDef(mdt) => {
                let func = self.get_function_by_rva(mdt.rva);
                let method_ref = self.assembly.image.get_method_by_rva(mdt.rva).unwrap();
                let method = method_ref.borrow();
                let method_sig = method.as_mdef().ty.as_fnptr().unwrap();
                if is_virtual {
                    callvirt(
                        self,
                        stack,
                        method
                            .get_class()
                            .borrow()
                            .get_method_index(method.get_name())
                            .unwrap(),
                        method_sig,
                        LLVMTypeOf(func),
                    );
                } else {
                    call(self, stack, func, method_sig);
                }
            }
            e => unimplemented!("{:?}", e),
        }
    }

    unsafe fn gen_instr_stfld(&mut self, stack: &mut Vec<TypedValue>, token: Token) {
        let val = stack.pop().unwrap();
        let obj = stack.pop().unwrap();
        match self.assembly.image.metadata.get_table_entry(token).unwrap() {
            Table::Field(f) => {
                let name = self.assembly.image.get_string(f.name);
                let class = &self.shared_env.ty_arena[obj.ty]
                    .as_class()
                    .unwrap()
                    .borrow();
                let idx = class.get_field_index(name).unwrap() + /*method_table=*/1;
                self.store2element(
                    obj.val,
                    vec![self.llvm_int32(0), self.llvm_int32(idx as u64)],
                    val.val,
                )
            }
            e => unimplemented!("{:?}", e),
        }
    }

    unsafe fn gen_instr_ldfld(&mut self, stack: &mut Vec<TypedValue>, token: Token) {
        let obj = stack.pop().unwrap();
        match self.assembly.image.metadata.get_table_entry(token).unwrap() {
            Table::Field(f) => {
                let name = self.assembly.image.get_string(f.name);
                let (idx, ty) = self.shared_env.ty_arena[obj.ty]
                    .as_class()
                    .unwrap()
                    .borrow()
                    .fields
                    .iter()
                    .enumerate()
                    .find(|(_, f)| &f.name == name)
                    .map(|(i, f)| (i, f.ty.clone()))
                    .unwrap();
                stack.push(TypedValue::new(
                    self.shared_env.type_id(&ty),
                    self.load_element(
                        obj.val,
                        vec![
                            self.llvm_int32(0),
                            self.llvm_int32(idx as u64 + /*method_table=*/1),
                        ],
                    ),
                ));
            }
            e => unimplemented!("{:?}", e),
        }
    }

    unsafe fn gen_instr_ldelem_i1(&mut self, stack: &mut Vec<TypedValue>) {
        let typed_val = self.gen_instr_general_ldelem(stack);
        stack.push(typed_val);
    }

    unsafe fn gen_instr_ldelem_i4(&mut self, stack: &mut Vec<TypedValue>) {
        let typed_val = self.gen_instr_general_ldelem(stack);
        stack.push(typed_val);
    }

    unsafe fn gen_instr_ldelem_ref(&mut self, stack: &mut Vec<TypedValue>) {
        let typed_val = self.gen_instr_general_ldelem(stack);
        stack.push(typed_val)
    }

    unsafe fn gen_instr_general_ldelem(&mut self, stack: &mut Vec<TypedValue>) -> TypedValue {
        let index = stack.pop().unwrap().val;
        let TypedValue {
            ty: arr_ty,
            val: array,
        } = stack.pop().unwrap();
        TypedValue::new(
            self.shared_env
                .type_id_with(|e| &e.ty_arena[arr_ty].as_szarray().unwrap().elem_ty),
            self.load_element(
                array,
                vec![LLVMBuildAdd(
                    self.shared_env.builder,
                    self.llvm_int32(1),
                    index,
                    cstr0!(),
                )],
            ),
        )
    }

    unsafe fn gen_instr_stelem_i1(&mut self, stack: &mut Vec<TypedValue>) {
        self.gen_instr_general_stelem(stack)
    }

    unsafe fn gen_instr_stelem_i4(&mut self, stack: &mut Vec<TypedValue>) {
        self.gen_instr_general_stelem(stack)
    }

    unsafe fn gen_instr_stelem_ref(&mut self, stack: &mut Vec<TypedValue>) {
        self.gen_instr_general_stelem(stack)
    }

    unsafe fn gen_instr_general_stelem(&mut self, stack: &mut Vec<TypedValue>) {
        let value = stack.pop().unwrap().val;
        let index = stack.pop().unwrap().val;
        let array = stack.pop().unwrap().val;
        self.store2element(
            array,
            vec![LLVMBuildAdd(
                self.shared_env.builder,
                self.llvm_int32(1),
                index,
                cstr0!(),
            )],
            value,
        );
    }

    unsafe fn gen_instr_ldlen(&mut self, stack: &mut Vec<TypedValue>) {
        let array = self.typecast(
            stack.pop().unwrap().val,
            LLVMPointerType(LLVMInt32TypeInContext(self.shared_env.context), 0),
        );
        let index = self.load_element(array, vec![self.llvm_int32(0)]);
        stack.push(TypedValue::new(
            self.shared_env.type_id(&Type::i4_ty()),
            index,
        ));
    }

    unsafe fn gen_instr_conv_r_un(&mut self, stack: &mut Vec<TypedValue>) {
        let value = stack.pop().unwrap().val;
        stack.push(TypedValue::new(
            self.shared_env.type_id(&Type::r8_ty()),
            LLVMBuildUIToFP(
                self.shared_env.builder,
                value,
                LLVMDoubleTypeInContext(self.shared_env.context),
                cstr0!(),
            ),
        ))
    }

    unsafe fn gen_instr_box(&mut self, stack: &mut Vec<TypedValue>, token: Token) {
        let val = stack.pop().unwrap().val;
        match self.assembly.image.metadata.get_table_entry(token).unwrap() {
            Table::TypeRef(_) => {
                let class_ref = self.assembly.image.get_class(token).unwrap().clone();
                let class = class_ref.borrow();
                let llvm_class = self.get_llvm_class_type(&class);
                let new_obj = self.typecast(
                    self.call_memory_alloc(self.get_size_of_llvm_class_type(llvm_class)),
                    llvm_class,
                );
                let (_, method_table) = self.ensure_all_class_methods_compiled(&*class);
                self.store2element(
                    new_obj,
                    vec![self.llvm_int32(0), self.llvm_int32(0)],
                    method_table,
                );
                self.store2element(new_obj, vec![self.llvm_int32(0), self.llvm_int32(1)], val);
                stack.push(TypedValue::new(
                    self.shared_env.type_id(&Type::object_ty()),
                    new_obj,
                ));
            }
            e => unimplemented!("newarr: unimplemented: {:?}", e),
        }
    }

    unsafe fn gen_instr_newarr(&mut self, stack: &mut Vec<TypedValue>, token: Token) {
        unsafe fn newarr_typeref<'a>(
            compiler: &mut JITCompiler,
            len: LLVMValueRef,
            trt: &TypeRefTable,
        ) -> TypedValue {
            let TypePath(path) = compiler.assembly.image.get_path_from_type_ref_table(trt);
            let ty = match (path[0], path[1], path[2]) {
                ("mscorlib", "System", ty) => ty,
                _ => unimplemented!(),
            };
            let (szarr_ty, sz) = match ty {
                "Int32" => (Type::i4_szarr_ty(), 4),
                "Boolean" => (Type::boolean_szarr_ty(), 1),
                "Object" => (Type::object_szarr_ty(), 8),
                _ => unimplemented!(),
            };
            let llvm_szarr_ty = szarr_ty.to_llvmty(compiler);
            let new_arr = compiler.typecast(
                compiler.call_function(
                    compiler
                        .shared_env
                        .methods
                        .get_helper_function("new_szarray")
                        .unwrap()
                        .llvm_function,
                    vec![compiler.llvm_int32(sz), len],
                ),
                llvm_szarr_ty,
            );
            TypedValue::new(compiler.shared_env.type_id(&szarr_ty), new_arr)
        }

        unsafe fn newarr_typedef<'a>(
            compiler: &mut JITCompiler,
            len: LLVMValueRef,
            token: Token,
        ) -> TypedValue {
            let elem_ty = Type::new(ElementType::Class(
                compiler.assembly.image.get_class(token).unwrap().clone(),
            ));
            let llvm_elem_ty = elem_ty.to_llvmty(compiler);
            let szarr_ty = Type::szarr_ty(elem_ty);
            let llvm_szarr_ty = szarr_ty.to_llvmty(compiler);
            let new_arr = compiler.typecast(
                compiler.call_function(
                    compiler
                        .shared_env
                        .methods
                        .get_helper_function("new_szarray")
                        .unwrap()
                        .llvm_function,
                    vec![compiler.get_size_of_llvm_class_type(llvm_elem_ty), len],
                ),
                llvm_szarr_ty,
            );
            TypedValue::new(compiler.shared_env.type_id(&szarr_ty), new_arr)
        }

        let len = stack.pop().unwrap().val;

        match self.assembly.image.metadata.get_table_entry(token).unwrap() {
            Table::TypeRef(trt) => stack.push(newarr_typeref(self, len, &trt)),
            Table::TypeDef(_) => stack.push(newarr_typedef(self, len, token)),
            e => unimplemented!("newarr: unimplemented: {:?}", e),
        }
    }

    unsafe fn gen_instr_newobj(&mut self, stack: &mut Vec<TypedValue>, token: Token) {
        match self.assembly.image.metadata.get_table_entry(token).unwrap() {
            Table::MemberRef(mrt) => {
                let class = self
                    .assembly
                    .image
                    .get_class(mrt.class_decoded())
                    .unwrap()
                    .clone();
                let class_borrowed = class.borrow();
                let method_name = self.assembly.image.get_string(mrt.name);
                let type_path = (&*class_borrowed).into(): TypePath;
                let method_ty = self
                    .assembly
                    .image
                    .get_method_ref_type_from_signature(mrt.signature);
                let (func, method_sig) = if let Some(f) = self
                    .shared_env
                    .methods
                    .get_method(type_path.with_method_name(method_name), &method_ty)
                {
                    (f.llvm_function, f.ty.as_fnptr().unwrap().clone())
                } else {
                    panic!();
                };
                let llvm_class_ty = self.get_llvm_class_type(&*class_borrowed);
                let new_obj = self.typecast(
                    self.call_memory_alloc(self.get_size_of_llvm_class_type(llvm_class_ty)),
                    llvm_class_ty,
                );
                let (_, mut args) = get_arg_vals_from_stack(stack, method_sig.params.len(), false);
                args.insert(0, new_obj);

                self.call_function(func, args);

                let (_, method_table) = self.ensure_all_class_methods_compiled(&*class_borrowed);

                self.store2element(
                    new_obj,
                    vec![self.llvm_int32(0), self.llvm_int32(0)],
                    method_table,
                );

                stack.push(TypedValue::new(
                    self.shared_env.type_id(&Type::class_ty(class.clone())),
                    new_obj,
                ))
            } // TODO
            Table::MethodDef(mdt) => {
                let method_ref = self.assembly.image.get_method_by_rva(mdt.rva).unwrap();
                let method_info = method_ref.borrow();
                let method = method_info.as_mdef();
                let method_sig = method.ty.as_fnptr().unwrap();
                let llvm_class_ty = self.get_llvm_class_type(&method.class.borrow());
                let new_obj = self.typecast(
                    self.call_memory_alloc(self.get_size_of_llvm_class_type(llvm_class_ty)),
                    llvm_class_ty,
                );
                let (_, mut args) = get_arg_vals_from_stack(stack, method_sig.params.len(), false);
                args.insert(0, new_obj);

                let func = self.get_function_by_rva(mdt.rva);
                self.call_function(func, args);

                let class = method.class.borrow();
                let (_, method_table) = self.ensure_all_class_methods_compiled(&*class);

                self.store2element(
                    new_obj,
                    vec![self.llvm_int32(0), self.llvm_int32(0)],
                    method_table,
                );

                stack.push(TypedValue::new(
                    self.shared_env
                        .type_id(&Type::class_ty(method.class.clone())),
                    new_obj,
                ))
            }
            e => unimplemented!("call: unimplemented: {:?}", e),
        }
    }

    unsafe fn call_function(&self, callee: LLVMValueRef, args: Vec<LLVMValueRef>) -> LLVMValueRef {
        let callee_ty = LLVMGetElementType(LLVMTypeOf(callee));
        let params_count = LLVMCountParamTypes(callee_ty) as usize;
        let mut params_ty = vec![0 as LLVMTypeRef; params_count];

        LLVMGetParamTypes(callee_ty, params_ty.as_mut_ptr());

        let mut conv_args: Vec<LLVMValueRef> = args
            .iter()
            .enumerate()
            .map(|(i, arg)| self.typecast(*arg, params_ty[i]))
            .collect();

        LLVMBuildCall(
            self.shared_env.builder,
            callee,
            conv_args.as_mut_ptr(),
            conv_args.len() as u32,
            cstr0!(),
        )
    }

    unsafe fn call_memory_alloc(&self, len: LLVMValueRef) -> LLVMValueRef {
        self.call_function(
            self.shared_env
                .methods
                .get_helper_function("memory_alloc")
                .unwrap()
                .llvm_function,
            vec![len],
        )
    }

    unsafe fn get_basic_block(&mut self, pc: usize) -> &mut BasicBlockInfo {
        let func = self.generating.unwrap();
        self.basic_blocks
            .entry(pc)
            .or_insert_with(|| BasicBlockInfo::Unpositioned(LLVMAppendBasicBlock(func, cstr0!())))
    }

    // TODO: &ClassInfo -> Into<TypePath>
    unsafe fn get_llvm_class_type(&mut self, class: &ClassInfo) -> LLVMTypeRef {
        // Return already created one
        if let Some(ty) = self.shared_env.class_types.get(class) {
            return ty;
        }

        // Create new class

        let class_ty = LLVMStructCreateNamed(
            self.shared_env.context,
            CString::new(class.name.as_str()).unwrap().as_ptr(),
        );
        let class_ptr_ty = LLVMPointerType(class_ty, 0);

        self.shared_env.class_types.add(class, class_ptr_ty);

        let mut fields_ty = class
            .fields
            .iter()
            .map(|ClassField { ty, .. }| ty.to_llvmty(self))
            .collect::<Vec<LLVMTypeRef>>();

        // method_table always occupies the first field
        fields_ty.insert(
            0,
            LLVMPointerType(
                LLVMPointerType(LLVMInt8TypeInContext(self.shared_env.context), 0),
                0,
            ),
        );

        LLVMStructSetBody(class_ty, fields_ty.as_mut_ptr(), fields_ty.len() as u32, 0);

        class_ptr_ty
    }

    unsafe fn get_size_of_llvm_class_type(&self, class: LLVMTypeRef) -> LLVMValueRef {
        LLVMConstPtrToInt(
            LLVMConstGEP(
                LLVMConstNull(class),
                vec![self.llvm_int32(1)].as_mut_ptr(),
                1,
            ),
            LLVMInt32TypeInContext(self.shared_env.context),
        )
    }

    unsafe fn typecast(&self, val: LLVMValueRef, to: LLVMTypeRef) -> LLVMValueRef {
        let v_ty = LLVMTypeOf(val);

        if matches!(LLVMGetTypeKind(to), llvm::LLVMTypeKind::LLVMVoidTypeKind) {
            return val;
        }

        match LLVMGetTypeKind(v_ty) {
            llvm::LLVMTypeKind::LLVMIntegerTypeKind => match LLVMGetTypeKind(to) {
                llvm::LLVMTypeKind::LLVMIntegerTypeKind => {
                    let val_bw = LLVMGetIntTypeWidth(v_ty);
                    let to_bw = LLVMGetIntTypeWidth(to);
                    if val_bw < to_bw {
                        return LLVMBuildZExtOrBitCast(self.shared_env.builder, val, to, cstr0!());
                    }
                }
                llvm::LLVMTypeKind::LLVMDoubleTypeKind => {
                    return LLVMBuildSIToFP(self.shared_env.builder, val, to, cstr0!());
                }
                llvm::LLVMTypeKind::LLVMPointerTypeKind => {
                    return LLVMBuildIntToPtr(self.shared_env.builder, val, to, cstr0!());
                }
                _ => {}
            },
            llvm::LLVMTypeKind::LLVMDoubleTypeKind | llvm::LLVMTypeKind::LLVMFloatTypeKind => {
                return LLVMBuildFPToSI(self.shared_env.builder, val, to, cstr0!());
            }
            llvm::LLVMTypeKind::LLVMVoidTypeKind => return val,
            llvm::LLVMTypeKind::LLVMPointerTypeKind => match LLVMGetTypeKind(to) {
                llvm::LLVMTypeKind::LLVMIntegerTypeKind => {
                    return LLVMBuildPtrToInt(self.shared_env.builder, val, to, cstr0!());
                }
                _ => {}
            },
            _ => {}
        }

        LLVMBuildTruncOrBitCast(self.shared_env.builder, val, to, cstr0!())
    }

    unsafe fn ensure_all_class_methods_compiled(
        &mut self,
        class: &ClassInfo,
        // method_table_ptr: MethodTablePtrTy, method_table: &Vec<MethodInfoRef>,
    ) -> (MethodTablePtrTy, LLVMValueRef) {
        let method_table_ptr = self
            .shared_env
            .class_types
            .get_method_table_ptr(class)
            .unwrap();

        if let Some((llvm_method_table, _)) =
            self.shared_env.method_table_map.get(&method_table_ptr)
        {
            return (method_table_ptr, *llvm_method_table);
        }

        // let method_table = class.method_table;
        let llvm_method_table = self.llvm_ptr(method_table_ptr as *mut u8);
        let mut methods = vec![];

        for m in &class.method_table {
            match &*m.borrow() {
                MethodInfo::MDef(m) => methods.push(self.get_function_by_rva(m.rva)),
                MethodInfo::MRef(m) => {
                    let class = m.class.borrow();
                    methods.push(
                        self.shared_env
                            .methods
                            .get_method(
                                ((&*class).into(): TypePath).with_method_name(m.name.as_str()),
                                &m.ty,
                            )
                            .unwrap()
                            .llvm_function,
                    );
                }
            }
        }

        self.shared_env
            .method_table_map
            .insert(method_table_ptr, (llvm_method_table, methods));

        (method_table_ptr, llvm_method_table)
    }

    unsafe fn load_element(&self, obj: LLVMValueRef, mut idx: Vec<LLVMValueRef>) -> LLVMValueRef {
        let gep = LLVMBuildGEP(
            self.shared_env.builder,
            obj,
            idx.as_mut_ptr(),
            idx.len() as u32,
            cstr0!(),
        );
        LLVMBuildLoad(self.shared_env.builder, gep, cstr0!())
    }

    unsafe fn store2element(
        &self,
        obj: LLVMValueRef,
        mut idx: Vec<LLVMValueRef>,
        val: LLVMValueRef,
    ) {
        let gep = LLVMBuildGEP(
            self.shared_env.builder,
            obj,
            idx.as_mut_ptr(),
            idx.len() as u32,
            cstr0!(),
        );
        LLVMBuildStore(
            self.shared_env.builder,
            self.typecast(val, LLVMGetElementType(LLVMTypeOf(gep))),
            gep,
        );
    }

    unsafe fn llvm_int32(&self, n: u64) -> LLVMValueRef {
        llvm_const_int32(self.shared_env.context, n)
    }

    unsafe fn llvm_real(&self, f: f64) -> LLVMValueRef {
        llvm_const_real(self.shared_env.context, f)
    }

    unsafe fn llvm_ptr(&self, ptr: *mut u8) -> LLVMValueRef {
        llvm_const_ptr(self.shared_env.context, ptr)
    }
}

unsafe fn cur_bb_has_no_terminator(builder: LLVMBuilderRef) -> bool {
    LLVMIsATerminatorInst(LLVMGetLastInstruction(LLVMGetInsertBlock(builder))) == ptr::null_mut()
}

pub trait CastIntoLLVMType {
    unsafe fn to_llvmty<'a>(&self, compiler: &mut JITCompiler<'a>) -> LLVMTypeRef;
}

impl CastIntoLLVMType for Type {
    unsafe fn to_llvmty<'a>(&self, compiler: &mut JITCompiler<'a>) -> LLVMTypeRef {
        let ctx = compiler.shared_env.context;
        match self.base {
            ElementType::Void => LLVMVoidTypeInContext(ctx),
            ElementType::Boolean => LLVMInt8TypeInContext(ctx),
            ElementType::Char => LLVMInt32TypeInContext(ctx),
            ElementType::I4 => LLVMInt32TypeInContext(ctx),
            ElementType::U4 => LLVMInt32TypeInContext(ctx),
            ElementType::I8 => LLVMInt64TypeInContext(ctx),
            ElementType::R8 => LLVMDoubleTypeInContext(ctx),
            ElementType::String => compiler
                .shared_env
                .class_types
                .get(TypePath(vec!["mscorlib", "System", "String"]))
                .unwrap(),
            ElementType::SzArray(ref szarr) => {
                LLVMPointerType(szarr.elem_ty.to_llvmty(compiler), 0)
            }
            ElementType::Class(ref class) => {
                let class = &class.borrow();
                compiler.get_llvm_class_type(class)
            }
            ElementType::Object => compiler
                .shared_env
                .class_types
                .get(TypePath(vec!["mscorlib", "System", "Object"]))
                .unwrap(),
            ElementType::ValueType(ref class) => {
                let class = &class.borrow();
                if class.is_enum() {
                    LLVMInt32TypeInContext(ctx)
                } else {
                    unimplemented!()
                }
            }
            ElementType::Ptr(ref elem) => LLVMPointerType(elem.to_llvmty(compiler), 0),
            ElementType::FnPtr(_) => unimplemented!(),
        }
    }
}

impl BasicBlockInfo {
    pub fn retrieve(&self) -> LLVMBasicBlockRef {
        match self {
            BasicBlockInfo::Positioned(bb) | BasicBlockInfo::Unpositioned(bb) => *bb,
        }
    }

    pub fn set_positioned(&mut self) -> &Self {
        match self {
            BasicBlockInfo::Unpositioned(bb) => *self = BasicBlockInfo::Positioned(*bb),
            BasicBlockInfo::Positioned(_) => {}
        };
        self
    }

    pub fn is_positioned(&self) -> bool {
        match self {
            BasicBlockInfo::Positioned(_) => true,
            BasicBlockInfo::Unpositioned(_) => false,
        }
    }
}

impl CodeEnvironment {
    pub fn new() -> Self {
        CodeEnvironment {
            arguments: FxHashMap::default(),
            locals: FxHashMap::default(),
        }
    }
}

impl SharedEnvironment {
    pub fn new() -> Self {
        unsafe {
            let context = LLVMContextCreate();
            let module =
                LLVMModuleCreateWithNameInContext(CString::new("yacht").unwrap().as_ptr(), context);
            let builder = LLVMCreateBuilderInContext(context);
            let pass_mgr = LLVMCreatePassManager();

            llvm::transforms::scalar::LLVMAddReassociatePass(pass_mgr);
            llvm::transforms::scalar::LLVMAddGVNPass(pass_mgr);
            llvm::transforms::scalar::LLVMAddInstructionCombiningPass(pass_mgr);
            llvm::transforms::scalar::LLVMAddPromoteMemoryToRegisterPass(pass_mgr);
            llvm::transforms::scalar::LLVMAddTailCallEliminationPass(pass_mgr);
            llvm::transforms::scalar::LLVMAddJumpThreadingPass(pass_mgr);

            SharedEnvironment {
                context,
                module,
                builder,
                pass_mgr,
                methods: BuiltinFunctions::new(context, module),
                class_types: ClassTypesNameResolver::new(),
                method_table_map: FxHashMap::default(),
                ty_arena: id_arena::Arena::new(),
            }
        }
    }

    pub fn type_id(&mut self, ty: &Type) -> TypeId {
        for (id, t) in &self.ty_arena {
            if t == ty {
                return id;
            }
        }

        self.ty_arena.alloc(ty.clone())
    }

    pub fn type_id_with<F>(&mut self, f: F) -> TypeId
    where
        F: FnOnce(&SharedEnvironment) -> &Type,
    {
        let ty = f(self);

        for (id, t) in &self.ty_arena {
            if t == ty {
                return id;
            }
        }

        let ty_cloned = ty.clone();

        self.ty_arena.alloc(ty_cloned)
    }
}

impl AssemblyUniqueEnvironment {
    pub fn new() -> Self {
        AssemblyUniqueEnvironment {
            generated: FxHashMap::default(),
            compile_queue: VecDeque::new(),
        }
    }
}

impl TypedValue {
    pub fn new(ty: id_arena::Id<Type>, val: LLVMValueRef) -> Self {
        Self { ty, val }
    }
}

impl ClassTypesNameResolver {
    pub unsafe fn new() -> Self {
        Self {
            base: NameResolver::new(),
        }
    }

    pub fn get<'a, T: Into<TypePath<'a>>>(&self, path: T) -> Option<LLVMTypeRef> {
        Some((*self.base.get(path.into())?).0)
    }

    pub fn get_method_table_ptr<'a, T: Into<TypePath<'a>>>(
        &self,
        path: T,
    ) -> Option<MethodTablePtrTy> {
        Some((*self.base.get(path.into())?).1)
    }

    pub fn add(&mut self, class: &ClassInfo, ty: LLVMTypeRef) {
        self.base.add(
            class.into(): TypePath,
            (ty, alloc_raw_method_table(class.method_table.len())),
        );
    }
}

fn get_arg_vals_from_stack(
    stack: &mut Vec<TypedValue>,
    params_len: usize,
    has_this: bool,
) -> (Option<TypeId>, Vec<LLVMValueRef>) {
    let mut args: Vec<LLVMValueRef> = stack
        .drain((stack.len() - params_len)..)
        .map(|tv| tv.val)
        .collect();
    if has_this {
        let this = stack.pop().unwrap();
        args.insert(0, this.val);
        (Some(this.ty), args)
    } else {
        (None, args)
    }
}

unsafe fn llvm_const_int32(ctx: LLVMContextRef, n: u64) -> LLVMValueRef {
    LLVMConstInt(LLVMInt32TypeInContext(ctx), n, 1)
}

unsafe fn llvm_const_real(ctx: LLVMContextRef, f: f64) -> LLVMValueRef {
    LLVMConstReal(LLVMDoubleTypeInContext(ctx), f)
}

unsafe fn llvm_const_ptr(ctx: LLVMContextRef, p: *mut u8) -> LLVMValueRef {
    let ptr_as_int = LLVMConstInt(LLVMInt64TypeInContext(ctx), p as u64, 0);
    LLVMConstIntToPtr(ptr_as_int, LLVMPointerType(LLVMInt8TypeInContext(ctx), 0))
}
