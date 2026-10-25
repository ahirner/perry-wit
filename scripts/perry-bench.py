"""Attempt every upstream Perry TypeScript benchmark in Node and Wasmtime."""

import argparse
from collections import Counter
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import statistics
import subprocess
import sys
import time


ROOT_BENCHMARKS = ("bench_fibonacci", "bench_array_ops", "bench_string_ops", "bench_bitwise")


def execute(command, directory, log, timeout):
    def expired(*_):
        raise TimeoutError

    start = time.perf_counter()
    timed_out = False
    with log.with_suffix(".stdout").open("w") as stdout, log.with_suffix(".stderr").open("w") as stderr:
        with subprocess.Popen(command, cwd=directory, stdout=stdout, stderr=stderr, start_new_session=True) as process:
            previous = signal.signal(signal.SIGALRM, expired)
            signal.setitimer(signal.ITIMER_REAL, timeout)
            try:
                _, status, usage = os.wait4(process.pid, 0)
            except TimeoutError:
                timed_out = True
                os.killpg(process.pid, signal.SIGKILL)
                _, status, usage = os.wait4(process.pid, 0)
            finally:
                signal.setitimer(signal.ITIMER_REAL, 0)
                signal.signal(signal.SIGALRM, previous)
            process.returncode = os.waitstatus_to_exitcode(status)
    elapsed = (time.perf_counter() - start) * 1000
    stdout = log.with_suffix(".stdout").read_text(errors="replace")
    stderr = log.with_suffix(".stderr").read_text(errors="replace")
    if timed_out:
        raise RuntimeError(f"timed out after {timeout}s\n{stderr}")
    if process.returncode:
        reason = f"signal {signal.Signals(-process.returncode).name}" if process.returncode < 0 else f"exit {process.returncode}"
        raise RuntimeError(f"{reason}\n{stderr or stdout}")
    if any(line.startswith("ERROR:") for line in stdout.splitlines()):
        raise RuntimeError(stdout)
    peak_rss = usage.ru_maxrss * (1 if sys.platform == "darwin" else 1024)
    return elapsed, stdout, peak_rss


def markdown(report):
    counts = ", ".join(f"{count} {status}" for status, count in sorted(Counter(row["status"] for row in report["results"]).items()))
    lines = ["## Perry benchmarks", "",
             f"Perry `{report['revision'] or report['source']}` · Node `{report.get('node', '?')}` · `{report.get('wasmtime', '?')}`.", "",
             f"{len(report['results'])}/{report['selected']} entries attempted: {counts}. {report['runs']} samples per engine, stopping an engine after its first error.", "",
             "Times are medians in ms; **W/N < 1 favors Wasmtime**. Peak RSS is the maximum whole-process resident memory across samples, in MiB (includes runtime/JIT, not just Wasm linear memory). Timing uses upstream contracts where available, otherwise process wall time. Node runs the original TypeScript; wall times include type stripping. Entries without output contracts are unchecked and have no speedup claim. Very short internal timings have coarse millisecond resolution.", "",
             "| Benchmark | Status | Timing | Node ms | Wasmtime ms | W/N | Node MiB | Wasmtime MiB |",
             "| --- | --- | --- | ---: | ---: | ---: | ---: | ---: |"]
    for row in report["results"]:
        times = row.get("median_ms", {})
        memory = row.get("peak_rss_bytes", {})
        values = [f"{times[runtime]:.2f}" if runtime in times else "—" for runtime in ("node", "wasmtime")]
        ratio = f"{times['wasmtime'] / times['node']:.2f}" if row["status"] == "pass" else "—"
        peaks = [f"{memory[runtime] / 2**20:.1f}" if runtime in memory else "—" for runtime in ("node", "wasmtime")]
        status = row["status"] + (f" ({', '.join(row['errors'])})" if row.get("errors") else "")
        lines.append("| " + " | ".join([row["benchmark"], status, row.get("timing", "—"), *values, ratio, *peaks]) + " |")
    for row in report["results"]:
        if row.get("reason") and row["reason"] != "no upstream timing/output contract":
            lines.extend(["", f"<details><summary>{row['benchmark']}: {row['status']}</summary>", "",
                          "```text", row["reason"].replace("```", "'''"), "```", "", "</details>"])
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("benchmarks", nargs="*", help="names from --list, optionally ending in .ts")
    parser.add_argument("--source", type=Path, default=os.environ.get("PERRY_BENCHMARK_SOURCE"),
                        help="upstream source checkout (defaults to the flake's locked source)")
    parser.add_argument("--compiler", default="perry-wit", help="compiler executable or path")
    parser.add_argument("--list", action="store_true", help="list every upstream TypeScript file")
    parser.add_argument("--all", action="store_true", help="select every listed benchmark")
    parser.add_argument("--compile-only", action="store_true", help="probe compilation without running workloads")
    parser.add_argument("--runs", type=int, default=3, help="fresh processes per runtime (default: 3)")
    parser.add_argument("--timeout", type=float, default=60, help="seconds per subprocess (default: 60)")
    parser.add_argument("--out", type=Path, help="new directory for components, logs and results.json")
    args = parser.parse_args()
    if args.source is None:
        parser.error("use nix run .#perry-bench, nix develop, or supply --source")
    if args.runs < 1 or args.timeout <= 0:
        parser.error("--runs and --timeout must be positive")
    source = args.source.resolve()
    benchmarks = source / "benchmarks"
    sys.path.insert(0, str(benchmarks))
    from benchmark_time import TIMING_CONTRACTS, TimingContract, extract_time_ms
    from verify_benchmark_output import compare_stdout_files

    contracts = {f"suite/{name}": contract for name, contract in TIMING_CONTRACTS.items()}
    for name in ROOT_BENCHMARKS:
        contract = TimingContract("TOTAL", auxiliary_time_labels=("AVG",))
        TIMING_CONTRACTS[name] = contract
        contracts[name] = contract
    catalog = {path.relative_to(benchmarks).with_suffix("").as_posix(): path for path in benchmarks.rglob("*.ts")}
    if args.list:
        for name in sorted(catalog):
            contract = contracts.get(name)
            print(f"{name:42} {contract.time_label if contract and contract.time_label else 'process wall time'}")
        return 0
    if args.all and args.benchmarks:
        parser.error("choose --all or benchmark names")
    selected = sorted(catalog) if args.all else [name.removesuffix(".ts") for name in args.benchmarks]
    if not selected:
        parser.error("select benchmark names, --all, or --list")
    unknown = sorted(set(selected) - catalog.keys())
    if unknown:
        parser.error(f"unknown benchmark: {', '.join(unknown)}; use --list")
    compiler = shutil.which(args.compiler)
    if compiler is None:
        parser.error(f"compiler not found: {args.compiler}")
    compiler = str(Path(compiler).resolve())
    output = (args.out or Path("artifacts/perry-benchmarks") / datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S%fZ")).resolve()
    output.mkdir(parents=True, exist_ok=False)
    wit = output / "wit"
    wit.mkdir()
    (wit / "world.wit").write_text("package perry:benchmark; world benchmark { include wasi:cli/command@0.3.0; }\n")
    report = {
        "source": str(source),
        "revision": os.environ.get("PERRY_BENCHMARK_REVISION") if args.source == Path(os.environ.get("PERRY_BENCHMARK_SOURCE", "")) else None,
        "compiler": compiler,
        "runs": args.runs,
        "selected": len(set(selected)),
        "timeout_seconds": args.timeout,
        "results": [],
    }
    for runtime in ("node", "wasmtime"):
        _, version, _ = execute([runtime, "--version"], output, output / runtime, args.timeout)
        report[runtime] = version.strip()
    print(f"Perry source: {source}\nResults: {output}", flush=True)
    for name in dict.fromkeys(selected):
        directory = output / name
        directory.mkdir(parents=True, exist_ok=True)
        entry = catalog[name]
        row = {"benchmark": name, "samples": []}
        errors = {}
        try:
            row["source_sha256"] = hashlib.sha256(entry.read_bytes()).hexdigest()
            elapsed, _, _ = execute([compiler, str(entry), "--wit", str(wit), "--world", "benchmark", "-o", "benchmark.wasm"],
                                 directory, directory / "compile", args.timeout)
            row.update(compile_ms=elapsed, component_bytes=(directory / "benchmark.wasm").stat().st_size)
        except (RuntimeError, OSError) as error:
            errors["compile"] = str(error)
        row["status"] = "compiled"
        if not args.compile_only:
            contract = contracts.get(name)
            row["timing"] = "internal" if contract and contract.time_label else "process-wall"
            for run in range(1, args.runs + 1):
                for runtime in (("node", "wasmtime") if run % 2 else ("wasmtime", "node")):
                    if runtime in errors or (runtime == "wasmtime" and "compile" in errors):
                        continue
                    try:
                        command = (["node", str(entry)] if runtime == "node" else
                                   ["wasmtime", "run", "-C", "cache=n", "-S", "p3=y", "-W", "component-model-async=y", "benchmark.wasm"])
                        wall, stdout, peak_rss = execute(command, directory, directory / f"{runtime}-{run}", args.timeout)
                        elapsed = extract_time_ms(name, stdout) if contract and contract.time_label else wall
                        row["samples"].append({"runtime": runtime, "run": run, "elapsed_ms": elapsed, "wall_ms": wall,
                                               "peak_rss_bytes": peak_rss})
                    except (RuntimeError, ValueError, OSError) as error:
                        errors[runtime] = str(error)
                if not errors:
                    expected = directory / f"node-{run}.stdout"
                    actual = directory / f"wasmtime-{run}.stdout"
                    if contract is None:
                        verification = {"status": "unchecked", "reason": "no upstream timing/output contract"}
                    elif contract.time_label is None:
                        verification = {"status": "pass" if expected.read_text() == actual.read_text() else "fail",
                                        "reason": "compared complete stdout for externally timed fixture"}
                    else:
                        verification = compare_stdout_files(expected_path=expected, actual_path=actual, benchmark=name)
                        if verification["status"] == "pass" and not any(
                            not line.startswith(("BENCHMARK:", "ITERATIONS:")) for line in verification["expected_lines"]
                        ):
                            verification = {"status": "unchecked", "reason": "no workload result in stdout"}
                    row["verification"] = verification
                    if verification["status"] == "fail":
                        errors["verify"] = verification["reason"]
            for runtime in ("node", "wasmtime"):
                samples = [sample for sample in row["samples"] if sample["runtime"] == runtime]
                if samples:
                    row.setdefault("median_ms", {})[runtime] = statistics.median(sample["elapsed_ms"] for sample in samples)
                    row.setdefault("peak_rss_bytes", {})[runtime] = max(sample["peak_rss_bytes"] for sample in samples)
            if not errors:
                row["status"] = row["verification"]["status"]
                row["reason"] = row["verification"]["reason"] if row["status"] != "pass" else ""
                if any(sample["elapsed_ms"] <= 0 for sample in row["samples"]):
                    row.update(status="unscoreable", reason="zero elapsed time in at least one sample")
                elif row["status"] == "pass":
                    row["wasmtime_speedup"] = row["median_ms"]["node"] / row["median_ms"]["wasmtime"]
        if errors:
            row.update(status="failed", errors=errors, reason="\n\n".join(f"{stage}: {reason}" for stage, reason in errors.items()))
        report["results"].append(row)
        (output / "results.json").write_text(json.dumps(report, indent=2) + "\n")
        (output / "summary.md").write_text(markdown(report))
        print(f"{name:42} {row['status']:20}", row.get("reason", "").partition("\n")[0], flush=True)
    return int(any(row["status"] not in ("pass", "compiled") for row in report["results"]))


if __name__ == "__main__":
    raise SystemExit(main())
