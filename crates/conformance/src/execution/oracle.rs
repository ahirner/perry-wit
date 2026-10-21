//! Persistent Node oracle daemon, shared SDK generation, and bounded TypeScript batch checking.
use super::{Case, FIXTURE, INPUTS, Observation, Outcome, command, number};
use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Mutex, OnceLock,
        mpsc::{Receiver, RecvTimeoutError, channel},
    },
    thread,
    time::Duration,
};

/// Maximum number of `.ts` files checked per `tsc --project` invocation.
const TSC_BATCH_SIZE: usize = 512;

struct SharedSdk {
    _temp: tempfile::TempDir,
    handler_types: PathBuf,
    capabilities_dts: PathBuf,
    oracle_mjs: PathBuf,
}

fn shared_sdk() -> Result<&'static SharedSdk> {
    static SDK: OnceLock<SharedSdk> = OnceLock::new();
    if let Some(sdk) = SDK.get() {
        return Ok(sdk);
    }
    let temp = tempfile::tempdir()?;
    let handler_types = temp.path().join("handler-types");
    perry_wit::generate_sdk_files(&perry_wit::SdkOptions {
        wit_dir: Path::new(env!("CARGO_MANIFEST_DIR")).join("wit/handler"),
        world: Some("handler".into()),
        out_dir: handler_types.clone(),
        project_root: Some(temp.path().into()),
        entry: "case.ts".into(),
        initialize_tsconfig: false,
    })?;
    let capabilities_dts = temp.path().join("capabilities.d.ts");
    fs::write(
        &capabilities_dts,
        "/// <reference path=\"./handler-types/runtime.d.ts\" />\ndeclare module \"test:generated/control\" { export function release():void; }",
    )?;
    let oracle_mjs = temp.path().join("oracle.mjs");
    fs::write(&oracle_mjs, include_str!("../oracle.mjs"))?;
    let _ = SDK.set(SharedSdk {
        _temp: temp,
        handler_types,
        capabilities_dts,
        oracle_mjs,
    });
    Ok(SDK.get().expect("sdk initialized"))
}

pub(super) fn link_handler_types(directory: &Path) -> Result<()> {
    let target = directory.join("handler-types");
    if target.exists() || target.symlink_metadata().is_ok() {
        return Ok(());
    }
    let sdk = shared_sdk()?;
    #[cfg(unix)]
    std::os::unix::fs::symlink(&sdk.handler_types, &target)?;
    #[cfg(not(unix))]
    {
        fs::create_dir_all(&target)?;
        for entry in fs::read_dir(&sdk.handler_types)? {
            let entry = entry?;
            if entry.path().is_file() {
                fs::copy(entry.path(), target.join(entry.file_name()))?;
            }
        }
    }
    Ok(())
}

fn typechecked_cache() -> &'static Mutex<HashSet<String>> {
    static CACHE: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashSet::new()))
}

pub(super) fn is_typechecked(source: &str) -> bool {
    typechecked_cache()
        .lock()
        .expect("typecheck cache lock")
        .contains(source)
}

pub(super) fn run_tsc_batch(files: &[(PathBuf, String)], directory: &Path) -> Result<()> {
    let sdk = shared_sdk()?;
    for chunk in files.chunks(TSC_BATCH_SIZE) {
        let mut config_files = Vec::with_capacity(chunk.len() + 1);
        for (path, _) in chunk {
            config_files.push(path.as_path());
        }
        config_files.push(sdk.capabilities_dts.as_path());
        let config = serde_json::json!({
            "compilerOptions": {
                "noEmit": true,
                "strict": true,
                "target": "esnext",
                "module": "esnext",
                "skipLibCheck": true,
            },
            "files": config_files,
        });
        let tsconfig = tempfile::Builder::new()
            .prefix("tsc-batch-")
            .suffix(".json")
            .tempfile_in(directory)?;
        fs::write(tsconfig.path(), serde_json::to_vec(&config)?)?;
        let mut cmd = Command::new("tsc");
        cmd.current_dir(directory)
            .arg("--project")
            .arg(tsconfig.path());
        let checked = command(&mut cmd, directory, Duration::from_secs(60))?;
        ensure!(
            checked.success(),
            "source failed TypeScript: {}",
            String::from_utf8_lossy(&checked.stdout)
        );
        let mut cache = typechecked_cache().lock().expect("typecheck cache lock");
        for (_, source) in chunk {
            cache.insert(source.clone());
        }
    }
    Ok(())
}

enum DaemonLine {
    Line(String),
    Eof,
    Error(io::Error),
}

enum DaemonCall {
    Outcome(Outcome),
    Timeout,
    Disconnected(anyhow::Error),
}

struct NodeDaemon {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<DaemonLine>,
    _sandbox: tempfile::TempDir,
}

#[derive(Serialize)]
struct OracleCaseMeta<'a> {
    kind: &'static str,
    #[serde(rename = "payloadPath", skip_serializing_if = "Option::is_none")]
    payload_path: Option<&'a Path>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    truncated: bool,
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    status: u16,
}

fn is_zero_u16(v: &u16) -> bool {
    *v == 0
}

#[derive(Serialize)]
struct OracleRequest<'a> {
    #[serde(rename = "casePath")]
    case_path: &'a Path,
    #[serde(rename = "stdoutPath")]
    stdout_path: &'a Path,
    #[serde(rename = "stderrPath")]
    stderr_path: &'a Path,
    inputs: &'a [String],
    fixture: &'static str,
    #[serde(rename = "testCase")]
    test_case: OracleCaseMeta<'a>,
}

#[derive(Deserialize)]
struct OracleResponse {
    ok: bool,
    #[serde(default)]
    observations: Vec<Observation>,
    #[serde(rename = "hasStdout", default)]
    has_stdout: bool,
    #[serde(rename = "hasStderr", default)]
    has_stderr: bool,
    #[serde(default)]
    error: String,
}

impl NodeDaemon {
    fn spawn() -> Result<Self> {
        let sdk = shared_sdk()?;
        let sandbox = tempfile::Builder::new().prefix("perry-oracle-").tempdir()?;
        let mut child = Command::new("node")
            .args([
                "--disable-warning=ExperimentalWarning",
                "--harmony-temporal",
                "--experimental-strip-types",
            ])
            .arg(&sdk.oracle_mjs)
            .env("TZ", "UTC")
            .env("PERRY_ORACLE_SANDBOX", sandbox.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().context("node daemon stdin")?;
        let stdout = child.stdout.take().context("node daemon stdout")?;
        let (tx, lines) = channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => {
                        let _ = tx.send(DaemonLine::Eof);
                        break;
                    }
                    Ok(_) => {
                        if tx.send(DaemonLine::Line(line)).is_err() {
                            break;
                        }
                    }
                    Err(err) => {
                        let _ = tx.send(DaemonLine::Error(err));
                        break;
                    }
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            lines,
            _sandbox: sandbox,
        })
    }

    fn call(&mut self, req: &OracleRequest<'_>, timeout: Duration) -> DaemonCall {
        let write_result = (|| -> io::Result<()> {
            serde_json::to_writer(&mut self.stdin, req).map_err(io::Error::other)?;
            self.stdin.write_all(b"\n")?;
            self.stdin.flush()
        })();
        if let Err(err) = write_result {
            return DaemonCall::Disconnected(err.into());
        }
        let line = match self.lines.recv_timeout(timeout) {
            Ok(DaemonLine::Line(line)) => line,
            Ok(DaemonLine::Eof) => {
                return DaemonCall::Disconnected(anyhow!("node daemon closed stdout unexpectedly"));
            }
            Ok(DaemonLine::Error(err)) => return DaemonCall::Disconnected(err.into()),
            Err(RecvTimeoutError::Timeout) => return DaemonCall::Timeout,
            Err(RecvTimeoutError::Disconnected) => {
                return DaemonCall::Disconnected(anyhow!("node daemon reader disconnected"));
            }
        };
        let resp: OracleResponse =
            match serde_json::from_str(&line).context("Node daemon response") {
                Ok(resp) => resp,
                Err(err) => return DaemonCall::Disconnected(err),
            };
        if resp.ok {
            let stdout = if resp.has_stdout {
                match fs::read(req.stdout_path) {
                    Ok(bytes) => bytes,
                    Err(err) => return DaemonCall::Disconnected(err.into()),
                }
            } else {
                Vec::new()
            };
            let stderr = if resp.has_stderr {
                match fs::read(req.stderr_path) {
                    Ok(bytes) => bytes,
                    Err(err) => return DaemonCall::Disconnected(err.into()),
                }
            } else {
                Vec::new()
            };
            DaemonCall::Outcome(Outcome::Values {
                observations: resp.observations,
                stdout,
                stderr,
                max_fuel_used: 0,
                component_bytes: 0,
                fuel_used: Vec::new(),
            })
        } else {
            DaemonCall::Outcome(Outcome::Execute(resp.error))
        }
    }
}

impl Drop for NodeDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub(super) fn run_node_oracle(
    case_path: &Path,
    inputs: &[String],
    test_case: &Case,
    timeout: Duration,
) -> Result<Outcome> {
    static DAEMON: OnceLock<Mutex<Option<NodeDaemon>>> = OnceLock::new();
    let dir = case_path.parent().context("case directory")?;
    let payload_path = dir.join("payload.bin");
    let stdout_path = dir.join("oracle-stdout.bin");
    let stderr_path = dir.join("oracle-stderr.bin");
    let meta = match test_case {
        Case::Stream(s) => {
            fs::write(&payload_path, &s.bytes)?;
            OracleCaseMeta {
                kind: "stream",
                payload_path: Some(&payload_path),
                truncated: false,
                status: 0,
            }
        }
        Case::Fetch(f) => {
            fs::write(&payload_path, &f.bytes)?;
            OracleCaseMeta {
                kind: "fetch",
                payload_path: Some(&payload_path),
                truncated: f.truncated,
                status: f.status,
            }
        }
        Case::Handler(h) => {
            fs::write(&payload_path, &h.bytes)?;
            OracleCaseMeta {
                kind: "handler",
                payload_path: Some(&payload_path),
                truncated: false,
                status: 0,
            }
        }
        Case::Lifecycle(_) => OracleCaseMeta {
            kind: "lifecycle",
            payload_path: None,
            truncated: false,
            status: 0,
        },
        _ => OracleCaseMeta {
            kind: "program",
            payload_path: None,
            truncated: false,
            status: 0,
        },
    };
    let req = OracleRequest {
        case_path,
        stdout_path: &stdout_path,
        stderr_path: &stderr_path,
        inputs,
        fixture: FIXTURE,
        test_case: meta,
    };
    let mutex = DAEMON.get_or_init(|| Mutex::new(None));
    let mut guard = mutex.lock().expect("node daemon lock");
    let first_call = {
        let daemon = match &mut *guard {
            Some(daemon) => daemon,
            None => guard.insert(NodeDaemon::spawn()?),
        };
        daemon.call(&req, timeout)
    };
    match first_call {
        DaemonCall::Outcome(outcome) => Ok(outcome),
        DaemonCall::Timeout => {
            *guard = None;
            Ok(Outcome::Timeout)
        }
        DaemonCall::Disconnected(_) => {
            *guard = None;
            let daemon = guard.insert(NodeDaemon::spawn()?);
            match daemon.call(&req, timeout) {
                DaemonCall::Outcome(outcome) => Ok(outcome),
                DaemonCall::Timeout => {
                    *guard = None;
                    Ok(Outcome::Timeout)
                }
                DaemonCall::Disconnected(err) => {
                    *guard = None;
                    Err(err)
                }
            }
        }
    }
}

pub(super) fn encoded_inputs() -> &'static [String] {
    static ENCODED: OnceLock<Vec<String>> = OnceLock::new();
    ENCODED
        .get_or_init(|| INPUTS.map(number).to_vec())
        .as_slice()
}
