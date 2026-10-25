"""Run Perry's official full regression suite in Node and Wasmtime."""

from collections import Counter
import json
import os
from pathlib import Path
import re
import signal
import statistics
import subprocess
import sys


RUNS = 3
TIMEOUT = 60


def execute(command, directory, log):
    def expired(*_):
        raise TimeoutError

    timed_out = False
    with log.with_suffix(".stdout").open("w") as stdout, log.with_suffix(".stderr").open("w") as stderr:
        with subprocess.Popen(command, cwd=directory, stdout=stdout, stderr=stderr, start_new_session=True) as process:
            previous = signal.signal(signal.SIGALRM, expired)
            signal.setitimer(signal.ITIMER_REAL, TIMEOUT)
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
    stdout = log.with_suffix(".stdout").read_text(errors="replace")
    stderr = log.with_suffix(".stderr").read_text(errors="replace")
    if timed_out:
        raise RuntimeError(f"timed out after {TIMEOUT}s\n{stderr}")
    if process.returncode:
        reason = f"signal {signal.Signals(-process.returncode).name}" if process.returncode < 0 else f"exit {process.returncode}"
        raise RuntimeError(f"{reason}\n{stderr or stdout}")
    if any(line.startswith("ERROR:") for line in stdout.splitlines()):
        raise RuntimeError(stdout)
    peak_rss = usage.ru_maxrss * (1 if sys.platform == "darwin" else 1024)
    return stdout, peak_rss


def markdown(report):
    counts = ", ".join(f"{count} {status}" for status, count in sorted(Counter(row["status"] for row in report["results"]).items()))
    lines = ["## Perry benchmarks", "",
             f"Perry `{report['revision']}` · Node `{report['node']}` · `{report['wasmtime']}`.", "",
             f"Official `compare.sh --full` suite: {len(report['results'])}/{report['selected']} attempted; {counts}.", "",
             f"Times are medians of {RUNS} fresh processes using upstream internal timings and warmups; **W/N < 1 favors Wasmtime**. Correctness uses upstream's stdout verifier. Ratios require matching outputs and positive timings in every sample; short timings have millisecond resolution. Peak RSS is the maximum whole-process memory in MiB, including runtime/JIT. Each process has a {TIMEOUT}s timeout; an engine stops after its first failure.", "",
             "| Benchmark | Status | Node ms | Wasmtime ms | W/N | Node MiB | Wasmtime MiB |",
             "| --- | --- | ---: | ---: | ---: | ---: | ---: |"]
    for row in report["results"]:
        times = row.get("median_ms", {})
        memory = row.get("peak_rss_bytes", {})
        values = [f"{times[runtime]:.2f}" if runtime in times else "—" for runtime in ("node", "wasmtime")]
        ratio = f"{row['ratio']:.2f}" if "ratio" in row else "—"
        peaks = [f"{memory[runtime] / 2**20:.1f}" if runtime in memory else "—" for runtime in ("node", "wasmtime")]
        status = row["status"] + (f" ({', '.join(row['errors'])})" if row.get("errors") else "")
        lines.append("| " + " | ".join([row["benchmark"], status, *values, ratio, *peaks]) + " |")
    for row in report["results"]:
        if row.get("reason"):
            lines.extend(["", f"<details><summary>{row['benchmark']}: {row['status']}</summary>", "",
                          "```text", row["reason"].replace("```", "'''"), "```", "", "</details>"])
    return "\n".join(lines) + "\n"


def main():
    benchmarks = Path(os.environ["PERRY_BENCHMARK_SOURCE"]) / "benchmarks"
    sys.path.insert(0, str(benchmarks))
    from benchmark_time import extract_time_ms
    from verify_benchmark_output import compare_stdout_files

    selection = re.search(r'elif \[\[ \$FULL_MODE -eq 1 \]\]; then\s*(?:#[^\n]*\n\s*)*BENCHMARKS="([^"]+)"',
                          (benchmarks / "compare.sh").read_text())
    if selection is None:
        raise RuntimeError("cannot read the official full suite from upstream compare.sh")
    selected = selection[1].split()
    output = Path("artifacts/perry-bench").resolve()
    output.mkdir(parents=True, exist_ok=True)
    wit = output / "wit"
    wit.mkdir(exist_ok=True)
    (wit / "world.wit").write_text("package perry:benchmark; world benchmark { include wasi:cli/command@0.3.0; }\n")
    report = {
        "revision": os.environ["PERRY_BENCHMARK_REVISION"],
        "runs": RUNS,
        "selected": len(selected),
        "timeout_seconds": TIMEOUT,
        "results": [],
    }
    for runtime in ("node", "wasmtime"):
        version, _ = execute([runtime, "--version"], output, output / runtime)
        report[runtime] = version.strip()
    print(f"Perry benchmarks: {benchmarks}\nResults: {output}", flush=True)
    for filename in selected:
        entry = benchmarks / "suite" / filename
        name = entry.stem
        directory = output / name
        directory.mkdir(parents=True, exist_ok=True)
        row = {"benchmark": name, "samples": {"node": [], "wasmtime": []}}
        errors = {}
        try:
            execute(["perry-wit", str(entry), "--wit", str(wit), "--world", "benchmark", "-o", "benchmark.wasm"],
                    directory, directory / "compile")
        except (RuntimeError, OSError) as error:
            errors["compile"] = str(error)
        for run in range(1, RUNS + 1):
            for runtime in (("node", "wasmtime") if run % 2 else ("wasmtime", "node")):
                if runtime in errors or (runtime == "wasmtime" and "compile" in errors):
                    continue
                try:
                    command = (["node", str(entry)] if runtime == "node" else
                               ["wasmtime", "run", "-C", "cache=n", "-S", "p3=y", "-W", "component-model-async=y", "benchmark.wasm"])
                    stdout, peak_rss = execute(command, directory, directory / f"{runtime}-{run}")
                    row["samples"][runtime].append({"elapsed_ms": extract_time_ms(name, stdout), "peak_rss_bytes": peak_rss})
                except (RuntimeError, ValueError, OSError) as error:
                    errors[runtime] = str(error)
            if not errors:
                row["verification"] = compare_stdout_files(expected_path=directory / f"node-{run}.stdout",
                                                           actual_path=directory / f"wasmtime-{run}.stdout", benchmark=name)
                if row["verification"]["status"] != "pass":
                    errors["verify"] = row["verification"]["reason"]
                    break
        for runtime, samples in row["samples"].items():
            if samples:
                row.setdefault("median_ms", {})[runtime] = statistics.median(sample["elapsed_ms"] for sample in samples)
                row.setdefault("peak_rss_bytes", {})[runtime] = max(sample["peak_rss_bytes"] for sample in samples)
        row["status"] = "pass"
        if not errors and all(sample["elapsed_ms"] > 0 for samples in row["samples"].values() for sample in samples):
            row["ratio"] = row["median_ms"]["wasmtime"] / row["median_ms"]["node"]
        if errors:
            row.update(status="failed", errors=errors, reason="\n\n".join(f"{stage}: {reason}" for stage, reason in errors.items()))
        report["results"].append(row)
        (output / "results.json").write_text(json.dumps(report, indent=2) + "\n")
        (output / "summary.md").write_text(markdown(report))
        print(f"{name:42} {row['status']:20}", row.get("reason", "").partition("\n")[0], flush=True)
    return int(any(row["status"] != "pass" for row in report["results"]))


if __name__ == "__main__":
    raise SystemExit(main())
