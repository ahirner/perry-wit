use anyhow::Result;

/// Instantiate the compiled worker inside a component whose async entry starts and cancels it.
/// Wasmtime's host API cannot yet request guest task cancellation; a component caller can.
pub(crate) fn cancellation_caller(worker: &[u8], wait_for_ready: bool) -> Result<Vec<u8>> {
    let printed = wasmprinter::print_bytes(worker)?;
    let prelude = &printed[..printed
        .find("  (core module")
        .expect("component core module")];
    let mut bindings = String::new();
    let mut instance = 0;
    for payload in wasmparser::Parser::new(0).parse_all(worker) {
        match payload? {
            wasmparser::Payload::ModuleSection { .. } => break,
            wasmparser::Payload::ComponentImportSection(imports) => {
                for import in imports {
                    let import = import?;
                    bindings.push_str(&format!(
                        "(with {:?} (instance {instance}))",
                        import.name.name
                    ));
                    instance += 1;
                }
            }
            _ => {}
        }
    }
    let child = printed.replacen("(component", "(component $worker", 1);
    let control = if wait_for_ready {
        r#"
(type $control-type (instance (export "ready" (func (result bool)))))
(import "test:control/cancellation" (instance $control (type $control-type)))
(alias export $control "ready" (func $ready))
(core func $ready (canon lower (func $ready)))
"#
    } else {
        r#"
(core module $ready-module (func (export "ready") (result i32) i32.const 1))
(core instance $ready-instance (instantiate $ready-module))
(alias core export $ready-instance "ready" (core func $ready))
"#
    };
    Ok(wat::parse_str(format!(
        r#"
{prelude}
{control}
{child}
(instance $worker-instance (instantiate $worker {bindings}))
(alias export $worker-instance "run" (func $run))
(core func $start (canon lower (func $run) async))
(core func $cancel (canon subtask.cancel))
(core func $drop (canon subtask.drop))
(core func $return (canon task.return (result u32)))
(core module $caller
  (import "host" "start" (func $start (result i32)))
  (import "host" "cancel" (func $cancel (param i32) (result i32)))
  (import "host" "drop" (func $drop (param i32)))
  (import "host" "return" (func $return (param i32)))
  (import "host" "ready" (func $ready (result i32)))
  (global $handle (mut i32) (i32.const 0))
  (func $finish (result i32) (local $status i32)
    call $ready
    if (result i32)
      global.get $handle call $cancel local.set $status
      global.get $handle call $drop
      local.get $status call $return
      i32.const 0
    else i32.const 1 end)
  (func (export "run") (result i32)
    call $start i32.const 4 i32.shr_u global.set $handle
    call $finish)
  (func (export "callback") (param i32 i32 i32) (result i32) call $finish))
(core instance $host
  (export "start" (func $start)) (export "cancel" (func $cancel))
  (export "drop" (func $drop)) (export "return" (func $return)) (export "ready" (func $ready)))
(core instance $caller-instance (instantiate $caller (with "host" (instance $host))))
(type $result (func async (result u32)))
(alias core export $caller-instance "run" (core func $entry))
(alias core export $caller-instance "callback" (core func $callback))
(func (export "run") (type $result) (canon lift (core func $entry) async (callback $callback)))
)"#
    ))?)
}
