from pathlib import Path
import runpy
import signal
import sys
import tempfile
import unittest


execute = runpy.run_path(str(Path(__file__).with_name("perry-bench.py")))["execute"]


class ExecutionTests(unittest.TestCase):
    def test_peak_rss_belongs_to_each_child(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            _, _, large = execute([sys.executable, "-c", "data = bytearray(64 * 1024 * 1024)"], root, root / "large", 10)
            _, _, small = execute([sys.executable, "-c", "pass"], root, root / "small", 10)
            self.assertGreater(large, 64 * 1024 * 1024)
            self.assertLess(small + 32 * 1024 * 1024, large)

    def test_failures_preserve_diagnostics_and_restore_alarm(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            previous = signal.getsignal(signal.SIGALRM)
            with self.assertRaisesRegex(RuntimeError, "signal SIGTERM"):
                execute([sys.executable, "-c", "import os, signal; os.kill(os.getpid(), signal.SIGTERM)"],
                        root, root / "signal", 10)
            with self.assertRaisesRegex(RuntimeError, "timed out"):
                execute([sys.executable, "-c", "import sys, time; print('before timeout', file=sys.stderr, flush=True); time.sleep(60)"],
                        root, root / "timeout", 0.2)
            self.assertEqual((root / "timeout.stderr").read_text(), "before timeout\n")
            self.assertEqual(signal.getsignal(signal.SIGALRM), previous)
            self.assertEqual(signal.getitimer(signal.ITIMER_REAL), (0, 0))
            _, stdout, _ = execute([sys.executable, "-c", "print('recovered')"], root, root / "after", 10)
            self.assertEqual(stdout, "recovered\n")


if __name__ == "__main__":
    unittest.main()
