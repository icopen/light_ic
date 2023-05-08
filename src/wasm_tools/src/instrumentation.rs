//! This module is responsible for instrumenting wasm binaries on the Internet
//! Computer.
//!
//! It exports the function [`instrument`] which takes a Wasm binary and
//! injects some instrumentation that allows to:
//!  * Quantify the amount of execution every function of that module conducts.
//!    This quantity is approximated by the sum of cost of instructions executed
//!    on the taken execution path.
//!  * Verify that no successful `memory.grow` results in exceeding the
//!    available memory allocated to the canister.
//!
//! Moreover, it exports the function referred to by the `start` section under
//! the name `canister_start` and removes the section. (This is needed so that
//! we can run the initialization after we have set the instructions counter to
//! some value).
//!
//! After instrumentation any function of that module will only be able to
//! execute as long as at every reentrant basic block of its execution path, the
//! counter is verified to be above zero. Otherwise, the function will trap (via
//! calling a special system API call). If the function returns before the
//! counter overflows, the value of the counter is the initial value minus the
//! sum of cost of all executed instructions.
//!
//! In more details, first, it inserts up to five System API functions:
//!
//! ```wasm
//! (import "__" "out_of_instructions" (func (;0;) (func)))
//! (import "__" "update_available_memory" (func (;1;) ((param i32 i32) (result i32))))
//! (import "__" "try_grow_stable_memory" (func (;1;) ((param i64 i64 i32) (result i64))))
//! (import "__" "deallocate_pages" (func (;1;) ((param i64))))
//! (import "__" "internal_trap" (func (;1;) ((param i32))))
//! ```
//! Where the last three will only be inserted if Wasm-native stable memory is enabled.
//!
//! It then inserts (and exports) a global mutable counter:
//! ```wasm
//! (global (;0;) (mut i64) (i64.const 0))
//! (export "canister counter_instructions" (global 0)))
//! ```
//!
//! An additional function is also inserted to handle updates to the instruction
//! counter for bulk memory instructions whose cost can only be determined at
//! runtime:
//!
//! ```wasm
//! (func (;5;) (type 4) (param i32) (result i32)
//!   global.get 0
//!   local.get 0
//!   i64.extend_i32_u
//!   i64.sub
//!   global.set 0
//!   global.get 0
//!   i64.const 0
//!   i64.lt_s
//!   if  ;; label = @1
//!     call 0           # the `out_of_instructions` function
//!   end
//!   local.get 0)
//! ```
//!
//! The `counter_instructions` global should be set before the execution of
//! canister code. After execution the global can be read to determine the
//! number of instructions used.
//!
//! Moreover, it injects a decrementation of the instructions counter (by the
//! sum of cost of all instructions inside this block) at the beginning of every
//! non-reentrant block:
//!
//! ```wasm
//! global.get 0
//! i64.const 2
//! i64.sub
//! global.set 0
//! ```
//!
//! and a decrementation with a counter overflow check at the beginning of every
//! reentrant block (a function or a loop body):
//!
//! ```wasm
//! global.get 0
//! i64.const 8
//! i64.sub
//! global.set 0
//! global.get 0
//! i64.const 0
//! i64.lt_s
//! if  ;; label = @1
//!   (call x)
//! end
//! ```
//!
//! Before every bulk memory operation, a call is made to the function which
//! will decrement the instruction counter by the "size" argument of the bulk
//! memory instruction.
//!
//! Note that we omit checking for the counter overflow at the non-reentrant
//! blocks to optimize for performance. The maximal overflow in that case is
//! bound by the length of the longest execution path consisting of
//! non-reentrant basic blocks.
//!
//! # Wasm-native stable memory
//!
//! Two additional memories are inserted for stable memory. One is the actual
//! stable memory and the other is a bytemap to track dirty pages in the stable
//! memory.
//! Index of stable memory bytemap = index of stable memory + 1
//! ```wasm
//! (memory (export "stable_memory") i64 (i64.const 0) (i64.const MAX_STABLE_MEMORY_SIZE))
//! (memory (export "stable_memory_bytemap") i32 (i64.const STABLE_BYTEMAP_SIZE) (i64.const STABLE_BYTEMAP_SIZE))
//! ```
//!

// use super::system_api_replacements::replacement_functions;
// use super::validation::API_VERSION_IC0;
// use super::{InstrumentationOutput, Segments, SystemApiFunc};
// use ic_config::flag_status::FlagStatus;
// use ic_replicated_state::NumWasmPages;
// use ic_sys::PAGE_SIZE;
// use ic_types::{methods::WasmMethod, MAX_WASM_MEMORY_IN_BYTES};
// use ic_types::{NumInstructions, MAX_STABLE_MEMORY_IN_BYTES};
// use ic_wasm_types::{BinaryEncodedWasm, WasmError, WasmInstrumentationError};
// use wasmtime_environ::WASM_PAGE_SIZE;

// use crate::wasm_utils::wasm_transform::{self, Module};
// use crate::wasmtime_embedder::{
//     STABLE_BYTEMAP_MEMORY_NAME, STABLE_MEMORY_NAME, WASM_HEAP_BYTEMAP_MEMORY_NAME,
//     WASM_HEAP_MEMORY_NAME,
// };
// use wasmparser::{
//     BlockType, ConstExpr, Export, ExternalKind, FuncType, Global, GlobalType, Import, MemoryType,
//     Operator, Type, TypeRef, ValType,
// };

// use std::collections::BTreeMap;
// use std::convert::TryFrom;

use wasmparser::{Export, ExternalKind};

use crate::wasm_transform::Module;

// The indicies of injected function imports.
// pub(crate) enum InjectedImports {
//     OutOfInstructions = 0,
//     UpdateAvailableMemory = 1,
//     TryGrowStableMemory = 2,
//     DeallocatePages = 3,
//     InternalTrap = 4,
// }

// impl InjectedImports {
//     fn count(wasm_native_stable_memory: FlagStatus) -> usize {
//         if wasm_native_stable_memory == FlagStatus::Enabled {
//             5
//         } else {
//             2
//         }
//     }
// }

// // Gets the cost of an instruction.
// fn instruction_to_cost(i: &Operator) -> u64 {
//     match i {
//         // The following instructions are mostly signaling the start/end of code blocks,
//         // so we assign 0 cost to them.
//         Operator::Block { .. } => 0,
//         Operator::Else => 0,
//         Operator::End => 0,
//         Operator::Loop { .. } => 0,

//         // Default cost of an instruction is 1.
//         _ => 1,
//     }
// }

// Injects two system api functions:
//   * `out_of_instructions` which is called, whenever a message execution runs
//     out of instructions.
//   * `update_available_memory` which is called after a native `memory.grow` to
//     check whether the canister has enough available memory according to its
//     memory allocation.
//
// Note that these functions are injected as the first two imports, so that we
// can increment all function indices unconditionally by two. (If they would be
// added as the last two imports, we'd need to increment only non imported
// functions, since imported functions precede all others in the function index
// space, but this would be error-prone).

// const INSTRUMENTED_FUN_MODULE: &str = "__";
// const OUT_OF_INSTRUCTIONS_FUN_NAME: &str = "out_of_instructions";
// const UPDATE_MEMORY_FUN_NAME: &str = "update_available_memory";
// const TRY_GROW_STABLE_MEMORY_FUN_NAME: &str = "try_grow_stable_memory";
// const DEALLOCATE_PAGES_NAME: &str = "deallocate_pages";
// const INTERNAL_TRAP_FUN_NAME: &str = "internal_trap";
const TABLE_STR: &str = "table";
// const CANISTER_COUNTER_INSTRUCTIONS_STR: &str = "canister counter_instructions";
// const CANISTER_COUNTER_DIRTY_PAGES_STR: &str = "canister counter_dirty_pages";
// const CANISTER_START_STR: &str = "canister_start";

// /// There is one byte for each OS page in the wasm heap.
// const BYTEMAP_SIZE_IN_WASM_PAGES: u64 =
//     MAX_WASM_MEMORY_IN_BYTES / (PAGE_SIZE as u64) / (WASM_PAGE_SIZE as u64);

// const MAX_STABLE_MEMORY_IN_WASM_PAGES: u64 = MAX_STABLE_MEMORY_IN_BYTES / (WASM_PAGE_SIZE as u64);
/// There is one byte for each OS page in the stable memory.
// const STABLE_BYTEMAP_SIZE_IN_WASM_PAGES: u64 = MAX_STABLE_MEMORY_IN_WASM_PAGES / (PAGE_SIZE as u64);

// fn add_type(module: &mut Module, ty: Type) -> u32 {
//     let Type::Func(sig) = &ty;
//     for (idx, Type::Func(msig)) in module.types.iter().enumerate() {
//         if *msig == *sig {
//             return idx as u32;
//         }
//     }
//     module.types.push(ty);
//     (module.types.len() - 1) as u32
// }

// fn mutate_function_indices(module: &mut Module, f: impl Fn(u32) -> u32) {
//     for func_body in &mut module.code_sections {
//         for instr in &mut func_body.instructions {
//             match instr {
//                 Operator::Call { function_index }
//                 | Operator::ReturnCall { function_index }
//                 | Operator::RefFunc { function_index } => {
//                     *function_index = f(*function_index);
//                 }
//                 _ => {}
//             }
//         }
//     }
//     for exp in &mut module.exports {
//         if let ExternalKind::Func = exp.kind {
//             exp.index = f(exp.index);
//         }
//     }
//     for (_, elem_items) in &mut module.elements {
//         if let wasm_transform::ElementItems::Functions(fun_items) = elem_items {
//             for idx in fun_items {
//                 *idx = f(*idx);
//             }
//         }
//     }
//     if let Some(start_idx) = module.start.as_mut() {
//         *start_idx = f(*start_idx);
//     }
// }

// fn inject_helper_functions(mut module: Module, wasm_native_stable_memory: FlagStatus) -> Module {
//     // insert types
//     let ooi_type = Type::Func(FuncType::new([], []));
//     let uam_type = Type::Func(FuncType::new([ValType::I32, ValType::I32], [ValType::I32]));

//     let ooi_type_idx = add_type(&mut module, ooi_type);
//     let uam_type_idx = add_type(&mut module, uam_type);

//     // push_front imports
//     let ooi_imp = Import {
//         module: INSTRUMENTED_FUN_MODULE,
//         name: OUT_OF_INSTRUCTIONS_FUN_NAME,
//         ty: TypeRef::Func(ooi_type_idx),
//     };

//     let uam_imp = Import {
//         module: INSTRUMENTED_FUN_MODULE,
//         name: UPDATE_MEMORY_FUN_NAME,
//         ty: TypeRef::Func(uam_type_idx),
//     };

//     let mut old_imports = module.imports;
//     module.imports =
//         Vec::with_capacity(old_imports.len() + InjectedImports::count(wasm_native_stable_memory));
//     module.imports.push(ooi_imp);
//     module.imports.push(uam_imp);

//     if wasm_native_stable_memory == FlagStatus::Enabled {
//         let tgsm_type = Type::Func(FuncType::new(
//             [ValType::I64, ValType::I64, ValType::I32],
//             [ValType::I64],
//         ));
//         let dp_type = Type::Func(FuncType::new([ValType::I64], []));
//         let tgsm_type_idx = add_type(&mut module, tgsm_type);
//         let dp_type_idx = add_type(&mut module, dp_type);
//         let tgsm_imp = Import {
//             module: INSTRUMENTED_FUN_MODULE,
//             name: TRY_GROW_STABLE_MEMORY_FUN_NAME,
//             ty: TypeRef::Func(tgsm_type_idx),
//         };
//         let dp_imp = Import {
//             module: INSTRUMENTED_FUN_MODULE,
//             name: DEALLOCATE_PAGES_NAME,
//             ty: TypeRef::Func(dp_type_idx),
//         };
//         module.imports.push(tgsm_imp);
//         module.imports.push(dp_imp);

//         let it_type = Type::Func(FuncType::new([ValType::I32], []));
//         let it_type_idx = add_type(&mut module, it_type);
//         let it_imp = Import {
//             module: INSTRUMENTED_FUN_MODULE,
//             name: INTERNAL_TRAP_FUN_NAME,
//             ty: TypeRef::Func(it_type_idx),
//         };
//         module.imports.push(it_imp);
//     }

//     module.imports.append(&mut old_imports);

//     // now increment all function references by InjectedImports::Count
//     let cnt = InjectedImports::count(wasm_native_stable_memory) as u32;
//     mutate_function_indices(&mut module, |i| i + cnt);

//     debug_assert!(
//         module.imports[InjectedImports::OutOfInstructions as usize].name == "out_of_instructions"
//     );
//     debug_assert!(
//         module.imports[InjectedImports::UpdateAvailableMemory as usize].name
//             == "update_available_memory"
//     );
//     if wasm_native_stable_memory == FlagStatus::Enabled {
//         debug_assert!(
//             module.imports[InjectedImports::TryGrowStableMemory as usize].name
//                 == "try_grow_stable_memory"
//         );
//         debug_assert!(
//             module.imports[InjectedImports::DeallocatePages as usize].name == "deallocate_pages"
//         );
//         debug_assert!(
//             module.imports[InjectedImports::InternalTrap as usize].name == "internal_trap"
//         );
//     }

//     module
// }

// #[derive(Default)]
// pub struct ExportModuleData {
//     pub instructions_counter_ix: u32,
//     pub dirty_pages_counter_ix: Option<u32>,
//     pub decr_instruction_counter_fn: u32,
//     pub count_clean_pages_fn: Option<u32>,
//     pub start_fn_ix: Option<u32>,
// }

/// Takes a Wasm binary and inserts the instructions metering and memory grow
/// instrumentation.
///
/// Returns an [`InstrumentationOutput`] or an error if the input binary could
/// not be instrumented.
// pub(super) fn instrument(
//     module: Module<'_>,
//     cost_to_compile_wasm_instruction: NumInstructions,
//     write_barrier: FlagStatus,
//     wasm_native_stable_memory: FlagStatus,
// ) -> Result<InstrumentationOutput, WasmInstrumentationError> {
//     let stable_memory_index;
//     let mut module = inject_helper_functions(module, wasm_native_stable_memory);
//     module = export_table(module);
//     (module, stable_memory_index) =
//         update_memories(module, write_barrier, wasm_native_stable_memory);

//     let mut extra_strs: Vec<String> = Vec::new();
//     module = export_mutable_globals(module, &mut extra_strs);

//     let mut num_imported_functions = 0;
//     let mut num_imported_globals = 0;
//     for imp in &module.imports {
//         match imp.ty {
//             TypeRef::Func(_) => {
//                 num_imported_functions += 1;
//             }
//             TypeRef::Global(_) => {
//                 num_imported_globals += 1;
//             }
//             _ => (),
//         }
//     }

//     let num_functions = (module.functions.len() + num_imported_functions) as u32;
//     let num_globals = (module.globals.len() + num_imported_globals) as u32;

//     let dirty_pages_counter_ix;
//     let count_clean_pages_fn;
//     match wasm_native_stable_memory {
//         FlagStatus::Enabled => {
//             dirty_pages_counter_ix = Some(num_globals + 1);
//             count_clean_pages_fn = Some(num_functions + 1);
//         }
//         FlagStatus::Disabled => {
//             dirty_pages_counter_ix = None;
//             count_clean_pages_fn = None;
//         }
//     };

//     let export_module_data = ExportModuleData {
//         instructions_counter_ix: num_globals,
//         dirty_pages_counter_ix,
//         decr_instruction_counter_fn: num_functions,
//         count_clean_pages_fn,
//         start_fn_ix: module.start,
//     };

//     if export_module_data.start_fn_ix.is_some() {
//         module.start = None;
//     }

//     // inject instructions counter decrementation
//     for func_body in &mut module.code_sections {
//         inject_metering(&mut func_body.instructions, &export_module_data);
//     }

//     // Collect all the function types of the locally defined functions inside the
//     // module.
//     //
//     // The main reason to create this vector of function types is because we can't
//     // mix a mutable (to inject instructions) and immutable (to look up the function
//     // type) reference to the `code_section`.
//     let mut func_types = Vec::new();
//     for i in 0..module.code_sections.len() {
//         let Type::Func(t) = &module.types[module.functions[i] as usize];
//         func_types.push(t.clone());
//     }

//     // Inject `update_available_memory` to functions with `memory.grow`
//     // instructions.
//     if !func_types.is_empty() {
//         let func_bodies = &mut module.code_sections;
//         for (func_ix, func_type) in func_types.into_iter().enumerate() {
//             inject_update_available_memory(&mut func_bodies[func_ix], &func_type);
//             if write_barrier == FlagStatus::Enabled {
//                 inject_mem_barrier(&mut func_bodies[func_ix], &func_type);
//             }
//         }
//     }

//     let mut extra_data: Option<Vec<u8>> = None;
//     module = export_additional_symbols(
//         module,
//         &export_module_data,
//         &mut extra_data,
//         wasm_native_stable_memory,
//         stable_memory_index + 1,
//     );

//     if wasm_native_stable_memory == FlagStatus::Enabled {
//         replace_system_api_functions(
//             &mut module,
//             stable_memory_index,
//             export_module_data.count_clean_pages_fn.unwrap(),
//             export_module_data.dirty_pages_counter_ix.unwrap(),
//         )
//     }

//     let exported_functions = module
//         .exports
//         .iter()
//         .filter_map(|export| WasmMethod::try_from(export.name.to_string()).ok())
//         .collect();

//     let expected_memories =
//         1 + match write_barrier {
//             FlagStatus::Enabled => 1,
//             FlagStatus::Disabled => 0,
//         } + match wasm_native_stable_memory {
//             FlagStatus::Enabled => 2,
//             FlagStatus::Disabled => 0,
//         };
//     if module.memories.len() > expected_memories {
//         return Err(WasmInstrumentationError::IncorrectNumberMemorySections {
//             expected: expected_memories,
//             got: module.memories.len(),
//         });
//     }

//     let initial_limit = if module.memories.is_empty() {
//         // if Wasm does not declare any memory section (mostly tests), use this default
//         0
//     } else {
//         module.memories[0].initial
//     };

//     // pull out the data from the data section
//     let data = get_data(&mut module.data)?;
//     data.validate(NumWasmPages::from(initial_limit as usize))?;

//     let mut wasm_instruction_count: u64 = 0;
//     for body in &module.code_sections {
//         wasm_instruction_count += body.instructions.len() as u64;
//     }
//     for glob in &module.globals {
//         wasm_instruction_count += glob.init_expr.get_operators_reader().into_iter().count() as u64;
//     }

//     let result = module.encode().map_err(|err| {
//         WasmInstrumentationError::WasmSerializeError(WasmError::new(err.to_string()))
//     })?;

//     Ok(InstrumentationOutput {
//         exported_functions,
//         data,
//         binary: BinaryEncodedWasm::new(result),
//         compilation_cost: cost_to_compile_wasm_instruction * wasm_instruction_count,
//     })
// }

// fn calculate_api_indexes(module: &Module<'_>) -> BTreeMap<SystemApiFunc, u32> {
//     module
//         .imports
//         .iter()
//         .filter(|imp| matches!(imp.ty, TypeRef::Func(_)))
//         .enumerate()
//         .filter_map(|(func_index, import)| {
//             if import.module == API_VERSION_IC0 {
//                 // The imports get function indexes before defined functions (so
//                 // starting at zero) and these are required to fit in 32-bits.
//                 SystemApiFunc::from_import_name(import.name).map(|api| (api, func_index as u32))
//             } else {
//                 None
//             }
//         })
//         .collect()
// }

// fn replace_system_api_functions(
//     module: &mut Module<'_>,
//     stable_memory_index: u32,
//     count_clean_pages_fn_index: u32,
//     dirty_pages_counter_index: u32,
// ) {
//     let api_indexes = calculate_api_indexes(module);
//     let number_of_func_imports = module
//         .imports
//         .iter()
//         .filter(|i| matches!(i.ty, TypeRef::Func(_)))
//         .count();

//     // Collect a single map of all the function indexes that need to be
//     // replaced.
//     let mut func_index_replacements = BTreeMap::new();
//     for (api, (ty, body)) in replacement_functions(
//         stable_memory_index,
//         count_clean_pages_fn_index,
//         dirty_pages_counter_index,
//     ) {
//         if let Some(old_index) = api_indexes.get(&api) {
//             let type_idx = add_type(module, ty);
//             let new_index = (number_of_func_imports + module.functions.len()) as u32;
//             module.functions.push(type_idx);
//             module.code_sections.push(body);
//             func_index_replacements.insert(*old_index, new_index);
//         }
//     }

//     // Perform all the replacements in a single pass.
//     mutate_function_indices(module, |idx| {
//         *func_index_replacements.get(&idx).unwrap_or(&idx)
//     });
// }

// Helper function used by instrumentation to export additional symbols.
//
// Returns the new module or panics in debug mode if a symbol is not reserved.
#[doc(hidden)] // pub for usage in tests
               // pub fn export_additional_symbols<'a>(
               //     mut module: Module<'a>,
               //     export_module_data: &ExportModuleData,
               //     extra_data: &'a mut Option<Vec<u8>>,
               //     wasm_native_stable_memory: FlagStatus,
               //     stable_memory_bytemap_index: u32,
               // ) -> Module<'a> {
               //     // push function to decrement the instruction counter

//     let func_type = Type::Func(FuncType::new([ValType::I32], [ValType::I32]));

//     use Operator::*;

//     let instructions = vec![
//         // Subtract the parameter amount from the instruction counter
//         GlobalGet {
//             global_index: export_module_data.instructions_counter_ix,
//         },
//         LocalGet { local_index: 0 },
//         I64ExtendI32U,
//         I64Sub,
//         GlobalSet {
//             global_index: export_module_data.instructions_counter_ix,
//         },
//         // Call out_of_instructions() if `counter < 0`.
//         GlobalGet {
//             global_index: export_module_data.instructions_counter_ix,
//         },
//         I64Const { value: 0 },
//         I64LtS,
//         If {
//             blockty: BlockType::Empty,
//         },
//         Call {
//             function_index: InjectedImports::OutOfInstructions as u32,
//         },
//         End,
//         // Return the original param so this function doesn't alter the stack
//         LocalGet { local_index: 0 },
//         End,
//     ];

//     let func_body = wasm_transform::Body {
//         locals: vec![],
//         instructions,
//     };

//     let type_idx = add_type(&mut module, func_type);
//     module.functions.push(type_idx);
//     module.code_sections.push(func_body);

//     if wasm_native_stable_memory == FlagStatus::Enabled {
//         // function to count dirty pages in a given range
//         let func_type = Type::Func(FuncType::new([ValType::I32, ValType::I32], [ValType::I32]));
//         let it = 2; // iterator index
//         let acc = 3; // accumulator index
//         let instructions = vec![
//             I32Const { value: 0 },
//             LocalSet { local_index: acc },
//             LocalGet { local_index: 0 },
//             LocalSet { local_index: it },
//             Loop {
//                 blockty: BlockType::Empty,
//             },
//             LocalGet { local_index: it },
//             // TODO read in bigger chunks (i64Load)
//             I32Load8U {
//                 memarg: wasmparser::MemArg {
//                     align: 0,
//                     max_align: 0,
//                     offset: 0,
//                     memory: stable_memory_bytemap_index,
//                 },
//             },
//             LocalGet { local_index: acc },
//             I32Add,
//             LocalSet { local_index: acc },
//             LocalGet { local_index: it },
//             I32Const { value: 1 },
//             I32Add,
//             LocalTee { local_index: it },
//             LocalGet { local_index: 1 },
//             I32LtU,
//             BrIf { relative_depth: 0 },
//             End,
//             // clean pages = len - dirty_count
//             LocalGet { local_index: 1 },
//             LocalGet { local_index: 0 },
//             I32Sub,
//             LocalGet { local_index: acc },
//             I32Sub,
//             End,
//         ];
//         let func_body = wasm_transform::Body {
//             locals: vec![(2, ValType::I32)],
//             instructions,
//         };
//         let type_idx = add_type(&mut module, func_type);
//         module.functions.push(type_idx);
//         module.code_sections.push(func_body);
//     }

//     // globals must be exported to be accessible to hypervisor or persisted
//     let counter_export = Export {
//         name: CANISTER_COUNTER_INSTRUCTIONS_STR,
//         kind: ExternalKind::Global,
//         index: export_module_data.instructions_counter_ix,
//     };
//     debug_assert!(super::validation::RESERVED_SYMBOLS.contains(&counter_export.name));
//     module.exports.push(counter_export);

//     if let Some(index) = export_module_data.dirty_pages_counter_ix {
//         let export = Export {
//             name: CANISTER_COUNTER_DIRTY_PAGES_STR,
//             kind: ExternalKind::Global,
//             index,
//         };
//         debug_assert!(super::validation::RESERVED_SYMBOLS.contains(&export.name));
//         module.exports.push(export);
//     }

//     if let Some(index) = export_module_data.start_fn_ix {
//         // push canister_start
//         let start_export = Export {
//             name: CANISTER_START_STR,
//             kind: ExternalKind::Func,
//             index,
//         };
//         debug_assert!(super::validation::RESERVED_SYMBOLS.contains(&start_export.name));
//         module.exports.push(start_export);
//     }

//     let mut zero_init_data: Vec<u8> = Vec::new();
//     use wasm_encoder::Encode;
//     //encode() automatically adds an End instructions
//     wasm_encoder::ConstExpr::i64_const(0).encode(&mut zero_init_data);
//     debug_assert!(extra_data.is_none());
//     *extra_data = Some(zero_init_data);

//     // push the instructions counter
//     module.globals.push(Global {
//         ty: GlobalType {
//             content_type: ValType::I64,
//             mutable: true,
//         },
//         init_expr: ConstExpr::new(extra_data.as_ref().unwrap(), 0),
//     });

//     if wasm_native_stable_memory == FlagStatus::Enabled {
//         // push the dirty page counter
//         module.globals.push(Global {
//             ty: GlobalType {
//                 content_type: ValType::I64,
//                 mutable: true,
//             },
//             init_expr: ConstExpr::new(extra_data.as_ref().unwrap(), 0),
//         });
//     }

//     module
// }

// Represents a hint about the context of each static cost injection point in
// wasm.
// #[derive(Copy, Clone, Debug, PartialEq)]
// enum Scope {
//     ReentrantBlockStart,
//     NonReentrantBlockStart,
//     BlockEnd,
// }

// Describes how to calculate the instruction cost at this injection point.
// `StaticCost` injection points contain information about the cost of the
// following basic block. `DynamicCost` injection points assume there is an i32
// on the stack which should be decremented from the instruction counter.
// #[derive(Copy, Clone, Debug, PartialEq)]
// enum InjectionPointCostDetail {
//     StaticCost { scope: Scope, cost: u64 },
//     DynamicCost,
// }

// impl InjectionPointCostDetail {
//     /// If the cost is statically known, increment it by the given amount.
//     /// Otherwise do nothing.
//     fn increment_cost(&mut self, additonal_cost: u64) {
//         match self {
//             Self::StaticCost { scope: _, cost } => *cost += additonal_cost,
//             Self::DynamicCost => {}
//         }
//     }
// }

// Represents a instructions metering injection point.
// #[derive(Copy, Clone, Debug)]
// struct InjectionPoint {
//     cost_detail: InjectionPointCostDetail,
//     position: usize,
// }

// impl InjectionPoint {
//     fn new_static_cost(position: usize, scope: Scope) -> Self {
//         InjectionPoint {
//             cost_detail: InjectionPointCostDetail::StaticCost { scope, cost: 0 },
//             position,
//         }
//     }

//     fn new_dynamic_cost(position: usize) -> Self {
//         InjectionPoint {
//             cost_detail: InjectionPointCostDetail::DynamicCost,
//             position,
//         }
//     }
// }

// This function iterates over the injection points, and inserts three different
// pieces of Wasm code:
// - we insert a simple instructions counter decrementation in a beginning of
//   every non-reentrant block
// - we insert a counter decrementation and an overflow check at the beginning
//   of every reentrant block (a loop or a function call).
// - we insert a function call before each dynamic cost instruction which
//   performs an overflow check and then decrements the counter by the value at
//   the top of the stack.
// fn inject_metering(code: &mut Vec<Operator>, export_data_module: &ExportModuleData) {
//     let points = injections(code);
//     let points = points.iter().filter(|point| match point.cost_detail {
//         InjectionPointCostDetail::StaticCost {
//             scope: Scope::ReentrantBlockStart,
//             cost: _,
//         } => true,
//         InjectionPointCostDetail::StaticCost { scope: _, cost } => cost > 0,
//         InjectionPointCostDetail::DynamicCost => true,
//     });
//     let orig_elems = code;
//     let mut elems: Vec<Operator> = Vec::new();
//     let mut last_injection_position = 0;

//     use Operator::*;

//     for point in points {
//         elems.extend_from_slice(&orig_elems[last_injection_position..point.position]);
//         match point.cost_detail {
//             InjectionPointCostDetail::StaticCost { scope, cost } => {
//                 elems.extend_from_slice(&[
//                     GlobalGet {
//                         global_index: export_data_module.instructions_counter_ix,
//                     },
//                     I64Const { value: cost as i64 },
//                     I64Sub,
//                     GlobalSet {
//                         global_index: export_data_module.instructions_counter_ix,
//                     },
//                 ]);
//                 if scope == Scope::ReentrantBlockStart {
//                     elems.extend_from_slice(&[
//                         GlobalGet {
//                             global_index: export_data_module.instructions_counter_ix,
//                         },
//                         I64Const { value: 0 },
//                         I64LtS,
//                         If {
//                             blockty: BlockType::Empty,
//                         },
//                         Call {
//                             function_index: InjectedImports::OutOfInstructions as u32,
//                         },
//                         End,
//                     ]);
//                 }
//             }
//             InjectionPointCostDetail::DynamicCost => {
//                 elems.extend_from_slice(&[Call {
//                     function_index: export_data_module.decr_instruction_counter_fn,
//                 }]);
//             }
//         }
//         last_injection_position = point.position;
//     }
//     elems.extend_from_slice(&orig_elems[last_injection_position..]);
//     *orig_elems = elems;
// }

// This function adds mem barrier writes, assuming that arguments
// of the original store operation are on the stack
// fn write_barrier_instructions<'a>(
//     offset: u64,
//     val_arg_idx: u32,
//     addr_arg_idx: u32,
// ) -> Vec<Operator<'a>> {
//     use Operator::*;
//     let page_size_shift = PAGE_SIZE.trailing_zeros() as i32;
//     let tracking_mem_idx = 1;
//     if offset % PAGE_SIZE as u64 == 0 {
//         vec![
//             LocalSet {
//                 local_index: val_arg_idx,
//             }, // value
//             LocalTee {
//                 local_index: addr_arg_idx,
//             }, // address
//             I32Const {
//                 value: page_size_shift,
//             },
//             I32ShrU,
//             I32Const { value: 1 },
//             I32Store8 {
//                 memarg: wasmparser::MemArg {
//                     align: 0,
//                     max_align: 0,
//                     offset: offset >> page_size_shift,
//                     memory: tracking_mem_idx,
//                 },
//             },
//             // Put original params on the stack
//             LocalGet {
//                 local_index: addr_arg_idx,
//             },
//             LocalGet {
//                 local_index: val_arg_idx,
//             },
//         ]
//     } else {
//         vec![
//             LocalSet {
//                 local_index: val_arg_idx,
//             }, // value
//             LocalTee {
//                 local_index: addr_arg_idx,
//             }, // address
//             I32Const {
//                 value: offset as i32,
//             },
//             I32Add,
//             I32Const {
//                 value: page_size_shift,
//             },
//             I32ShrU,
//             I32Const { value: 1 },
//             I32Store8 {
//                 memarg: wasmparser::MemArg {
//                     align: 0,
//                     max_align: 0,
//                     offset: 0,
//                     memory: tracking_mem_idx,
//                 },
//             },
//             // Put original params on the stack
//             LocalGet {
//                 local_index: addr_arg_idx,
//             },
//             LocalGet {
//                 local_index: val_arg_idx,
//             },
//         ]
//     }
// }

// fn inject_mem_barrier(func_body: &mut wasm_transform::Body, func_type: &FuncType) {
//     use Operator::*;
//     let mut injection_points: Vec<usize> = Vec::new();
//     {
//         for (idx, instr) in func_body.instructions.iter().enumerate() {
//             match instr {
//                 I32Store { .. } | I32Store8 { .. } | I32Store16 { .. } => {
//                     injection_points.push(idx)
//                 }
//                 I64Store { .. } | I64Store8 { .. } | I64Store16 { .. } | I64Store32 { .. } => {
//                     injection_points.push(idx)
//                 }
//                 F32Store { .. } => injection_points.push(idx),
//                 F64Store { .. } => injection_points.push(idx),
//                 _ => (),
//             }
//         }
//     }

//     // If we found some injection points, we need to instrument the code.
//     if !injection_points.is_empty() {
//         // We inject some locals to cache the arguments to `memory.store`.
//         // The locals are stored as a vector of (count, ValType), so summing over the first field gives
//         // the total number of locals.
//         let n_locals: u32 = func_body.locals.iter().map(|x| x.0).sum();
//         let arg_i32_addr_idx = func_type.params().len() as u32 + n_locals;
//         let arg_i32_val_idx = arg_i32_addr_idx + 1;
//         func_body.locals.push((2, ValType::I32));
//         let arg_i64_val_idx = arg_i32_val_idx + 1;
//         func_body.locals.push((1, ValType::I64));
//         let arg_f32_val_idx = arg_i64_val_idx + 1;
//         func_body.locals.push((1, ValType::F32));
//         let arg_f64_val_idx = arg_f32_val_idx + 1;
//         func_body.locals.push((1, ValType::F64));

//         let orig_elems = &func_body.instructions;
//         let mut elems: Vec<Operator> = Vec::new();
//         let mut last_injection_position = 0;
//         for point in injection_points {
//             let mem_instr = orig_elems[point].clone();
//             elems.extend_from_slice(&orig_elems[last_injection_position..point]);

//             match mem_instr {
//                 I32Store { memarg } | I32Store8 { memarg } | I32Store16 { memarg } => {
//                     elems.extend_from_slice(&write_barrier_instructions(
//                         memarg.offset,
//                         arg_i32_val_idx,
//                         arg_i32_addr_idx,
//                     ));
//                 }
//                 I64Store { memarg }
//                 | I64Store8 { memarg }
//                 | I64Store16 { memarg }
//                 | I64Store32 { memarg } => {
//                     elems.extend_from_slice(&write_barrier_instructions(
//                         memarg.offset,
//                         arg_i64_val_idx,
//                         arg_i32_addr_idx,
//                     ));
//                 }
//                 F32Store { memarg } => {
//                     elems.extend_from_slice(&write_barrier_instructions(
//                         memarg.offset,
//                         arg_f32_val_idx,
//                         arg_i32_addr_idx,
//                     ));
//                 }
//                 F64Store { memarg } => {
//                     elems.extend_from_slice(&write_barrier_instructions(
//                         memarg.offset,
//                         arg_f64_val_idx,
//                         arg_i32_addr_idx,
//                     ));
//                 }
//                 _ => {}
//             }
//             // add the original store instruction itself
//             elems.push(mem_instr);

//             last_injection_position = point + 1;
//         }
//         elems.extend_from_slice(&orig_elems[last_injection_position..]);
//         func_body.instructions = elems;
//     }
// }

// Scans through a function and adds instrumentation after each `memory.grow`
// instruction to make sure that there's enough available memory left to support
// the requested extra memory. If no `memory.grow` instructions are present then
// the function's code remains unchanged.
// fn inject_update_available_memory(func_body: &mut wasm_transform::Body, func_type: &FuncType) {
//     use Operator::*;
//     let mut injection_points: Vec<usize> = Vec::new();
//     {
//         for (idx, instr) in func_body.instructions.iter().enumerate() {
//             // TODO(EXC-222): Once `table.grow` is supported we should extend the list of
//             // injections here.
//             if let MemoryGrow { .. } = instr {
//                 injection_points.push(idx);
//             }
//         }
//     }

//     // If we found any injection points, we need to instrument the code.
//     if !injection_points.is_empty() {
//         // We inject a local to cache the argument to `memory.grow`.
//         // The locals are stored as a vector of (count, ValType), so summing over the first field gives
//         // the total number of locals.
//         let n_locals: u32 = func_body.locals.iter().map(|x| x.0).sum();
//         let memory_local_ix = func_type.params().len() as u32 + n_locals;
//         func_body.locals.push((1, ValType::I32));

//         let orig_elems = &func_body.instructions;
//         let mut elems: Vec<Operator> = Vec::new();
//         let mut last_injection_position = 0;
//         for point in injection_points {
//             let update_available_memory_instr = orig_elems[point].clone();
//             elems.extend_from_slice(&orig_elems[last_injection_position..point]);
//             // At this point we have a memory.grow so the argument to it will be on top of
//             // the stack, which we just assign to `memory_local_ix` with a local.tee
//             // instruction.
//             elems.extend_from_slice(&[
//                 LocalTee {
//                     local_index: memory_local_ix,
//                 },
//                 update_available_memory_instr,
//                 LocalGet {
//                     local_index: memory_local_ix,
//                 },
//                 Call {
//                     function_index: InjectedImports::UpdateAvailableMemory as u32,
//                 },
//             ]);
//             last_injection_position = point + 1;
//         }
//         elems.extend_from_slice(&orig_elems[last_injection_position..]);
//         func_body.instructions = elems;
//     }
// }

// This function scans through the Wasm code and creates an injection point
// at the beginning of every basic block (straight-line sequence of instructions
// with no branches) and before each bulk memory instruction. An injection point
// contains a "hint" about the context of every basic block, specifically if
// it's re-entrant or not.
// fn injections(code: &[Operator]) -> Vec<InjectionPoint> {
//     let mut res = Vec::new();
//     let mut stack = Vec::new();
//     use Operator::*;
//     // The function itself is a re-entrant code block.
//     let mut curr = InjectionPoint::new_static_cost(0, Scope::ReentrantBlockStart);
//     for (position, i) in code.iter().enumerate() {
//         curr.cost_detail.increment_cost(instruction_to_cost(i));
//         match i {
//             // Start of a re-entrant code block.
//             Loop { .. } => {
//                 stack.push(curr);
//                 curr = InjectionPoint::new_static_cost(position + 1, Scope::ReentrantBlockStart);
//             }
//             // Start of a non re-entrant code block.
//             If { .. } | Block { .. } => {
//                 stack.push(curr);
//                 curr = InjectionPoint::new_static_cost(position + 1, Scope::NonReentrantBlockStart);
//             }
//             // End of a code block but still more code left.
//             Else | Br { .. } | BrIf { .. } | BrTable { .. } => {
//                 res.push(curr);
//                 curr = InjectionPoint::new_static_cost(position + 1, Scope::BlockEnd);
//             }
//             // `End` signals the end of a code block. If there's nothing more on the stack, we've
//             // gone through all the code.
//             End => {
//                 res.push(curr);
//                 curr = match stack.pop() {
//                     Some(val) => val,
//                     None => break,
//                 };
//             }
//             // Bulk memory instructions require injected metering __before__ the instruction
//             // executes so that size arguments can be read from the stack at runtime.
//             MemoryFill { .. }
//             | MemoryCopy { .. }
//             | MemoryInit { .. }
//             | TableCopy { .. }
//             | TableInit { .. } => {
//                 res.push(InjectionPoint::new_dynamic_cost(position));
//             }
//             // Nothing special to be done for other instructions.
//             _ => (),
//         }
//     }

//     res.sort_by_key(|k| k.position);
//     res
// }

// Looks for the data section and if it is present, converts it to a vector of
// tuples (heap offset, bytes) and then deletes the section.
// fn get_data(
//     data_section: &mut Vec<wasm_transform::DataSegment>,
// ) -> Result<Segments, WasmInstrumentationError> {
//     let res = data_section
//         .iter()
//         .map(|segment| {
//             let offset = match &segment.kind {
//                 wasm_transform::DataSegmentKind::Active {
//                     memory_index: _,
//                     offset_expr,
//                 } => match offset_expr {
//                     Operator::I32Const { value } => *value as usize,
//                     _ => return Err(WasmInstrumentationError::WasmDeserializeError(WasmError::new(
//                         "complex initialization expressions for data segments are not supported!".into()
//                     ))),
//                 },

//                 _ => return Err(WasmInstrumentationError::WasmDeserializeError(
//                     WasmError::new("no offset found for the data segment".into())
//                 )),
//             };

//             Ok((offset, segment.data.to_vec()))
//         })
//         .collect::<Result<_,_>>()?;

//     data_section.clear();
//     Ok(res)
// }

pub fn export_table(mut module: Module) -> Module {
    let mut table_already_exported = false;
    for export in &mut module.exports {
        if let ExternalKind::Table = export.kind {
            table_already_exported = true;
            export.name = TABLE_STR;
        }
    }

    if !table_already_exported && !module.tables.is_empty() {
        let table_export = Export {
            name: TABLE_STR,
            kind: ExternalKind::Table,
            index: 0,
        };
        module.exports.push(table_export);
    }

    module
}

// / Exports existing memories and injects new memories. Returns the index of an
// / injected stable memory when using wasm-native stable memory. The bytemap for
// / the stable memory will always be inserted directly after the stable memory.
// fn update_memories(
//     mut module: Module,
//     write_barrier: FlagStatus,
//     wasm_native_stable_memory: FlagStatus,
// ) -> (Module, u32) {
//     let mut stable_index = 0;

//     let mut memory_already_exported = false;
//     for export in &mut module.exports {
//         if let ExternalKind::Memory = export.kind {
//             memory_already_exported = true;
//             export.name = WASM_HEAP_MEMORY_NAME;
//         }
//     }

//     if !memory_already_exported && !module.memories.is_empty() {
//         let memory_export = Export {
//             name: WASM_HEAP_MEMORY_NAME,
//             kind: ExternalKind::Memory,
//             index: 0,
//         };
//         module.exports.push(memory_export);
//     }

//     if write_barrier == FlagStatus::Enabled && !module.memories.is_empty() {
//         module.memories.push(MemoryType {
//             memory64: false,
//             shared: false,
//             initial: BYTEMAP_SIZE_IN_WASM_PAGES,
//             maximum: Some(BYTEMAP_SIZE_IN_WASM_PAGES),
//         });

//         module.exports.push(Export {
//             name: WASM_HEAP_BYTEMAP_MEMORY_NAME,
//             kind: ExternalKind::Memory,
//             index: 1,
//         });
//     }

//     if wasm_native_stable_memory == FlagStatus::Enabled {
//         stable_index = module.memories.len() as u32;
//         module.memories.push(MemoryType {
//             memory64: true,
//             shared: false,
//             initial: 0,
//             maximum: Some(MAX_STABLE_MEMORY_IN_WASM_PAGES),
//         });

//         module.exports.push(Export {
//             name: STABLE_MEMORY_NAME,
//             kind: ExternalKind::Memory,
//             index: stable_index,
//         });

//         module.memories.push(MemoryType {
//             memory64: false,
//             shared: false,
//             initial: STABLE_BYTEMAP_SIZE_IN_WASM_PAGES,
//             maximum: Some(STABLE_BYTEMAP_SIZE_IN_WASM_PAGES),
//         });

//         module.exports.push(Export {
//             name: STABLE_BYTEMAP_MEMORY_NAME,
//             kind: ExternalKind::Memory,
//             // Bytemap for a memory needs to be placed at the next index after the memory
//             index: stable_index + 1,
//         })
//     }

//     (module, stable_index)
// }

// Mutable globals must be exported to be persisted.
// fn export_mutable_globals<'a>(
//     mut module: Module<'a>,
//     extra_data: &'a mut Vec<String>,
// ) -> Module<'a> {
//     let mut mutable_exported: Vec<(bool, bool)> = module
//         .globals
//         .iter()
//         .map(|g| g.ty.mutable)
//         .zip(std::iter::repeat(false))
//         .collect();

//     for export in &module.exports {
//         if let ExternalKind::Global = export.kind {
//             mutable_exported[export.index as usize].1 = true;
//         }
//     }

//     for (ix, (mutable, exported)) in mutable_exported.iter().enumerate() {
//         if *mutable && !exported {
//             extra_data.push(format!("__persistent_mutable_global_{}", ix));
//         }
//     }
//     let mut iy = 0;
//     for (ix, (mutable, exported)) in mutable_exported.into_iter().enumerate() {
//         if mutable && !exported {
//             let global_export = Export {
//                 name: extra_data[iy].as_str(),
//                 kind: ExternalKind::Global,
//                 index: ix as u32,
//             };
//             module.exports.push(global_export);
//             iy += 1;
//         }
//     }

//     module
// }
