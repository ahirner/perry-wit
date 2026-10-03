//! Connects generated HTTP resource bindings to a TypeScript handler and the guest work driver.

use std::collections::HashMap;
use std::convert::Infallible;
use std::fmt::Write;

use anyhow::{Context, Result, ensure};
use wasm_encoder::reencode::{self, Reencode};
use wasm_encoder::{CodeSection, ExportSection, Function, Instruction, Module};
use wasmparser::{ExportSectionReader, FunctionBody, Parser, TypeRef};

use super::trampoline::{DiscoveredExports, discover_module_exports};

const BRIDGES: [&str; 3] = ["begin", "invoke", "end"];

/// Emits ordinary guest boundaries without trapping before Rust releases HTTP resources.
pub(super) fn append_bridge(
    snippets: &mut String,
    discovered: &DiscoveredExports,
    target: u32,
) -> Result<()> {
    ensure!(
        discovered.function_types[target as usize].params() == [wasmparser::ValType::I64]
            && discovered.function_types[target as usize].results() == [wasmparser::ValType::I64],
        "incomingHandlerHandle must accept one Request and return a Response or Promise<Response>"
    );
    let start = discovered
        .start_func
        .context("incoming handler requires _start")?;
    let failed = discovered
        .user_functions
        .get("cabi_http_handler_failed")
        .context("missing handler exception bridge")?;
    let checkpoint = discovered
        .cabi_record_init_checkpoint
        .context("missing init checkpoint")?;
    let reclaim = discovered
        .cabi_reclaim_temporaries
        .context("missing handler reclamation bridge")?;
    let reclaim_http = discovered
        .http_reclaim_responses
        .map(|index| format!("call {index}"))
        .unwrap_or_default();
    write!(
        snippets,
        r#"
  (func $perry_http_begin (result i32)
    global.get $perry_init_guard
    i32.const -1
    i32.eq
    if
      i32.const 0
      return
    end
    global.get $perry_init_guard
    i32.eqz
    if
      call {start}
      call {failed}
      if
        i32.const -1
        global.set $perry_init_guard
        i32.const 0
        return
      end
      call {checkpoint}
      i32.const 1
      global.set $perry_init_guard
    end
    call $perry_safe_reset
    i32.const 1
  )
  (func $perry_http_invoke (param i64) (result i64)
    local.get 0
    call {target}
    call $perry_drain_work
  )
  (func $perry_http_end
    call $perry_scan_globals
    call {reclaim}
    {reclaim_http}
  )
"#
    )
    .unwrap();
    for name in BRIDGES {
        writeln!(
            snippets,
            "  (export \"__perry_http_{name}\" (func $perry_http_{name}))"
        )
        .unwrap();
    }
    Ok(())
}

/// Replaces runtime hooks, preserving calls from Rust and removing temporary linker exports.
pub(super) fn connect_bridge(bytes: &[u8]) -> Result<Vec<u8>> {
    let exports = discover_module_exports(bytes)?;
    let mut replacements = HashMap::new();
    for name in BRIDGES {
        let original = exports
            .user_functions
            .get(&format!("cabi_http_handler_{name}"))
            .context("missing runtime HTTP hook")?;
        let target = exports
            .user_functions
            .get(&format!("__perry_http_{name}"))
            .context("missing synthesized HTTP hook")?;
        ensure!(
            exports.function_types[*original as usize] == exports.function_types[*target as usize],
            "HTTP hook {name} has an incompatible runtime signature"
        );
        replacements.insert(
            *original,
            (
                *target,
                exports.function_types[*target as usize].params().len() as u32,
            ),
        );
    }
    let mut imported_functions = 0;
    for payload in Parser::new(0).parse_all(bytes) {
        if let wasmparser::Payload::ImportSection(reader) = payload? {
            for import in reader.into_imports() {
                imported_functions += u32::from(matches!(import?.ty, TypeRef::Func(_)));
            }
        }
    }
    let mut reencoder = HandlerHooks {
        replacements,
        next_function: imported_functions,
    };
    let mut module = Module::new();
    reencoder
        .parse_core_module(&mut module, Parser::new(0), bytes)
        .map_err(|error| anyhow::anyhow!("connecting HTTP handler: {error}"))?;
    Ok(module.finish())
}

/// Keeps generated resource ownership code intact while replacing only its execution hooks.
struct HandlerHooks {
    replacements: HashMap<u32, (u32, u32)>,
    next_function: u32,
}

impl Reencode for HandlerHooks {
    type Error = Infallible;

    fn parse_function_body(
        &mut self,
        code: &mut CodeSection,
        body: FunctionBody<'_>,
    ) -> Result<(), reencode::Error<Self::Error>> {
        if let Some(&(target, parameters)) = self.replacements.get(&self.next_function) {
            let mut function = Function::new([]);
            for parameter in 0..parameters {
                function.instruction(&Instruction::LocalGet(parameter));
            }
            function.instruction(&Instruction::Call(target));
            function.instruction(&Instruction::End);
            code.function(&function);
        } else {
            reencode::utils::parse_function_body(self, code, body)?;
        }
        self.next_function += 1;
        Ok(())
    }

    fn parse_export_section(
        &mut self,
        exports: &mut ExportSection,
        reader: ExportSectionReader<'_>,
    ) -> Result<(), reencode::Error<Self::Error>> {
        for export in reader {
            let export = export?;
            if !export.name.starts_with("__perry_http_") {
                exports.export(export.name, self.export_kind(export.kind)?, export.index);
            }
        }
        Ok(())
    }
}
