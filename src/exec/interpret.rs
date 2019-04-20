use crate::{
    exec::instruction::*,
    metadata::{metadata::*, method::MethodBodyRef, signature::*},
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value {
    Int32(i32),
    String(u32),
}

#[derive(Clone)]
pub struct Interpreter {
    stack: [Value; 1024],
    base_ptr: usize,
    stack_ptr: usize,
    program_counter: usize,
}

impl Interpreter {
    pub fn new() -> Self {
        Self {
            stack: [Value::Int32(0); 1024],
            base_ptr: 0,
            stack_ptr: 0,
            program_counter: 0,
        }
    }

    pub fn interpret(&mut self, image: &mut Image, method: MethodBodyRef, arguments: &[Value]) {
        macro_rules! numeric_op {
            ($op:ident) => {{
                let y = self.stack_pop();
                let x = self.stack_pop();
                self.stack_push(x.$op(y))
            }};
        }

        let iseq = &method.borrow().body;

        loop {
            let instr = &iseq[self.program_counter];
            match instr {
                Instruction::Ldstr { us_offset } => self.stack_push(Value::String(*us_offset)),
                Instruction::Ldc_I4_0 => self.stack_push(Value::Int32(0)),
                Instruction::Ldc_I4_1 => self.stack_push(Value::Int32(1)),
                Instruction::Ldc_I4_S { n } => self.stack_push(Value::Int32(*n)),
                Instruction::Ldarg_0 => self.stack_push(arguments[0]),
                Instruction::Ldarg_1 => self.stack_push(arguments[1]),
                Instruction::Pop => self.stack_ptr -= 1,
                Instruction::Bge { target } => self.instr_bge(image, *target),
                Instruction::Add => numeric_op!(add),
                Instruction::Call { table, entry } => self.instr_call(image, *table, *entry),
                Instruction::Ret => break,
            }
            self.program_counter += 1;
        }
    }

    #[inline]
    pub fn stack_push(&mut self, v: Value) {
        self.stack[self.base_ptr + self.stack_ptr] = v;
        self.stack_ptr += 1;
    }

    #[inline]
    pub fn stack_pop(&mut self) -> Value {
        self.stack_ptr -= 1;
        self.stack[self.base_ptr + self.stack_ptr]
    }

    #[inline]
    pub fn stack_pop_last_elements(&mut self, n: usize) -> Vec<Value> {
        self.stack_ptr -= n;
        self.stack[(self.base_ptr + self.stack_ptr)..].to_vec()
    }
}

impl Interpreter {
    fn instr_call(&mut self, image: &mut Image, table: usize, entry: usize) {
        // TODO: Refacotr
        let table = &image.metadata.metadata_stream.tables[table][entry - 1];
        match table {
            Table::MemberRef(mrt) => {
                let (table, entry) = mrt.class_table_and_entry();
                let class = &image.metadata.metadata_stream.tables[table][entry - 1];
                match class {
                    Table::TypeRef(trt) => {
                        let (table, entry) = trt.resolution_scope_table_and_entry();
                        let art = match image.metadata.metadata_stream.tables[table][entry - 1] {
                            Table::AssemblyRef(art) => art,
                            _ => unimplemented!(),
                        };
                        let ar_name = image.get_string(art.name);
                        let ty_namespace = image.get_string(trt.type_namespace);
                        let ty_name = image.get_string(trt.type_name);
                        let name = image.get_string(mrt.name);
                        let sig = image.metadata.blob.get(&(mrt.signature as u32)).unwrap();
                        let ty = SignatureParser::new(sig).parse_method_ref_sig().unwrap();

                        dprintln!(" [{}]{}.{}::{}", ar_name, ty_namespace, ty_name, name);

                        dprintln!("Method type: {:?}", ty);

                        if ar_name == "mscorlib"
                            && ty_namespace == "System"
                            && ty_name == "Console"
                            && name == "WriteLine"
                        {
                            let val = self.stack_pop();
                            if ty.equal_method(ElementType::Void, &[ElementType::String]) {
                                println!(
                                    "{}",
                                    String::from_utf16_lossy(
                                        image
                                            .metadata
                                            .user_strings
                                            .get(&val.as_string().unwrap())
                                            .unwrap()
                                    )
                                );
                            } else if ty.equal_method(ElementType::Void, &[ElementType::I4]) {
                                println!("{}", val.as_int32().unwrap());
                            }
                        }
                    }
                    _ => unimplemented!(),
                }
            }
            Table::MethodDef(mdt) => {
                let saved_program_counter = self.program_counter;
                self.program_counter = 0;

                let params = {
                    let sig = image.metadata.blob.get(&(mdt.signature as u32)).unwrap();
                    let ty = SignatureParser::new(sig).parse_method_def_sig().unwrap();
                    let method_sig = ty.as_fnptr().unwrap();
                    self.stack_pop_last_elements(method_sig.params.len() as usize)
                };
                let method_ref = image.get_method(mdt.rva);

                self.interpret(image, method_ref, &params);

                self.program_counter = saved_program_counter;
            }
            e => unimplemented!("call: unimplemented: {:?}", e),
        }
    }

    fn instr_bge(&mut self, _image: &mut Image, target: usize) {
        let val2 = self.stack_pop();
        let val1 = self.stack_pop();
        if val1.ge(val2) {
            self.program_counter = target /* interpret() everytime increments pc */- 1
        }
    }
}

impl Value {
    pub fn as_string(&self) -> Option<u32> {
        match self {
            Value::String(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_int32(&self) -> Option<i32> {
        match self {
            Value::Int32(n) => Some(*n),
            _ => None,
        }
    }

    pub fn add(self, y: Value) -> Value {
        match (self, y) {
            (Value::Int32(x), Value::Int32(y)) => Value::Int32(x + y),
            _ => panic!(),
        }
    }

    pub fn ge(self, y: Value) -> bool {
        match (self, y) {
            (Value::Int32(x), Value::Int32(y)) => x >= y,
            _ => panic!(),
        }
    }
}
