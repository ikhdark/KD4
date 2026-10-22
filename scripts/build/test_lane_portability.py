"""The Python lane owner uses native file locks on Windows and POSIX."""

import errno
import os
from pathlib import Path
from types import SimpleNamespace
import tempfile
import unittest
from unittest import mock

from scripts.build import rust_build_status as lanes


class LanePortabilityTest(unittest.TestCase):
    def test_coordination_lock_excludes_another_owner_and_releases_on_error(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            path = root / ".lane-coordination.lock"
            with self.assertRaisesRegex(RuntimeError, "body failed"):
                with lanes.cargo_lane_coordination_lock(root, timeout_seconds=0):
                    self.assertIsNone(lanes._try_acquire_binary_file_lock(path))
                    raise RuntimeError("body failed")
            handle = lanes._try_acquire_binary_file_lock(path)
            self.assertIsNotNone(handle)
            try:
                lanes._release_binary_file_lock(handle)
            finally:
                handle.close()

    def test_posix_owners_use_flock_and_retain_bounded_contention(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fake_fcntl = SimpleNamespace(LOCK_EX=2, LOCK_NB=4, LOCK_UN=8, flock=mock.Mock())
            # Replace only this module's os reference; Path retains the real host.
            with (
                mock.patch.object(lanes, "os", SimpleNamespace(name="posix", SEEK_END=os.SEEK_END)),
                mock.patch.dict("sys.modules", {"fcntl": fake_fcntl}),
            ):
                with lanes.cargo_lane_coordination_lock(root, timeout_seconds=0):
                    pass
                self.assertEqual([call.args[1] for call in fake_fcntl.flock.call_args_list], [6, 8])
                fake_fcntl.flock.reset_mock()
                (root / ".lane-active.lock").touch()
                self.assertFalse(lanes.lane_active_lock_is_held(root))
                self.assertEqual([call.args[1] for call in fake_fcntl.flock.call_args_list], [6, 8])
                fake_fcntl.flock.side_effect = BlockingIOError(errno.EAGAIN, "busy")
                self.assertTrue(lanes.lane_active_lock_is_held(root))
                with self.assertRaises(TimeoutError):
                    with lanes.cargo_lane_coordination_lock(root, timeout_seconds=0):
                        self.fail("contended ownership must never be admitted")


if __name__ == "__main__":
    unittest.main()
