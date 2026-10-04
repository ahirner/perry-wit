export function runRun(): {ok:true}|{ok:false} {
  // Conformance Test: Console Stream Routing
  // Capability: web.console

  console.log("stdout: stream line 1");
  console.error("stderr: stream line 1");
  console.log("stdout: stream line 2");
  return {ok:true};
}
