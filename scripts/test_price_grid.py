"""Exercise the shell driver's output-directory validation without running Cargo."""

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


class PriceGridOutputDirectoryTests(unittest.TestCase):
    def test_unsafe_output_directories_are_refused_before_cleanup(self):
        with tempfile.TemporaryDirectory() as scratch:
            parent = Path(scratch)
            repo = parent / "repo"
            scripts = repo / "scripts"
            scripts.mkdir(parents=True)
            script = scripts / "price_grid.sh"
            shutil.copyfile(Path(__file__).with_name(script.name), script)
            (repo / "alias").symlink_to(repo, target_is_directory=True)
            commands = parent / "commands"
            commands.mkdir()
            for name in ("rm", "mkdir", "cargo"):
                command = commands / name
                command.write_text(f'#!/bin/sh\necho "unexpected {name}" >&2\nexit 42\n')
                command.chmod(0o755)

            for directory in ("", ".", "/", str(repo), str(parent), "scripts/../..", "alias"):
                with self.subTest(directory=directory):
                    result = subprocess.run(
                        ["bash", str(script), "--pr"],
                        env={
                            **os.environ,
                            "PATH": f"{commands}{os.pathsep}{os.environ['PATH']}",
                            "OUT_DIR": directory,
                        },
                        capture_output=True,
                        text=True,
                    )
                    self.assertEqual(result.returncode, 2, result.stderr)
                    self.assertIn("unsafe OUT_DIR", result.stderr)
                    self.assertNotIn("unexpected", result.stderr)


if __name__ == "__main__":
    unittest.main()
