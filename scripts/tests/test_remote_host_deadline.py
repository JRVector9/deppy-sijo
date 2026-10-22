"""Use a fake Docker executable; never start or delete a real container."""
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
CONTAINER = "deppy-remote-host-build-" + "a" * 48


class ContainerDeadline(unittest.TestCase):
    def run_fixture(self, mode, container=CONTAINER):
        with tempfile.TemporaryDirectory(prefix="deppy-container-deadline-") as temp:
            directory = Path(temp)
            log = directory / "cleanup"
            docker = directory / "docker"
            docker.write_text("#!/bin/sh\ncase \"$1\" in run) if [ \"$DEPPY_TEST_MODE\" = hang ]; then exec sleep 30; else exit 17; fi;; rm) printf '%s\\n' \"$@\" > \"$DEPPY_TEST_CLEANUP\";; esac\n")
            docker.chmod(0o755)
            environment = dict(os.environ, PATH=str(directory) + os.pathsep + os.environ["PATH"], DEPPY_TEST_MODE=mode, DEPPY_TEST_CLEANUP=str(log))
            process = subprocess.Popen(["python3", "scripts/run-remote-host-container.py", "--container", container, "--seconds", "0.1" if mode == "hang" else "2", "--", "docker", "run", "--name", container], cwd=ROOT, env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
            try:
                stdout, stderr = process.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.communicate()
                self.fail("host deadline did not stop the hung container command")
            return process.returncode, stderr, log.read_text().splitlines() if log.exists() else []

    def test_timeout_stops_only_the_exact_task_owned_container(self):
        code, _, cleanup = self.run_fixture("hang")
        self.assertEqual(code, 124)
        self.assertEqual(cleanup, ["rm", "--force", CONTAINER])

    def test_completed_command_keeps_its_status_without_container_cleanup(self):
        code, _, cleanup = self.run_fixture("complete")
        self.assertEqual(code, 17)
        self.assertEqual(cleanup, [])

    def test_unrelated_container_names_are_rejected_before_docker(self):
        code, _, cleanup = self.run_fixture("hang", "unrelated-user-container")
        self.assertEqual(code, 2)
        self.assertEqual(cleanup, [])


if __name__ == "__main__":
    unittest.main()
