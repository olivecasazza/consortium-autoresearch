"""
Unit test for ClusterShell.Task in tree mode
"""

import logging
import os
from textwrap import dedent
import types
import unittest


from ClusterShell.Propagation import RouteResolvingError
from ClusterShell.Task import task_self
from ClusterShell.Topology import TopologyError

from TLib import HOSTNAME, make_temp_file

# live logging with nosetests --nologcapture
logging.basicConfig(level=logging.DEBUG)


class TreeTaskTest(unittest.TestCase):
    """Test cases for Tree-related Task methods"""

    def tearDown(self):
        """clear task topology and reset connect_timeout"""
        task = task_self()
        task.topology = None
        task.gateways = {}
        # restore default connect_timeout (10 s per Defaults.py)
        task._info["connect_timeout"] = 10

    def test_shell_auto_tree_dummy(self):
        """test task shell auto tree"""
        # initialize a dummy topology.conf file
        topofile = make_temp_file(dedent("""
                        [Main]
                        %s: dummy-gw
                        dummy-gw: dummy-node"""% HOSTNAME).encode())
        task = task_self()
        task.set_default("auto_tree", True)
        task.TOPOLOGY_CONFIGS = [topofile.name]

        self.assertRaises(RouteResolvingError, task.run, "/bin/hostname",
                          nodes="dummy-node", stderr=True)
        self.assertEqual(task.max_retcode(), None)

    def test_shell_auto_tree_noconf(self):
        """test task shell auto tree [no topology.conf]"""
        task = task_self()
        task.set_default("auto_tree", True)
        dummyfile = "/some/dummy/path/topo.conf"
        self.assertFalse(os.path.exists(dummyfile))
        task.TOPOLOGY_CONFIGS = [dummyfile]
        # do not raise exception
        task.run("/bin/hostname", nodes="dummy-node")

    def test_shell_auto_tree_error(self):
        """test task shell auto tree [TopologyError]"""
        # initialize an erroneous topology.conf file
        topofile = make_temp_file(dedent("""
                        [Main]
                        %s: dummy-gw
                        dummy-gw: dummy-gw"""% HOSTNAME).encode())
        task = task_self()
        task.set_default("auto_tree", True)
        task.TOPOLOGY_CONFIGS = [topofile.name]
        self.assertRaises(TopologyError, task.run, "/bin/hostname",
                          nodes="dummy-node")

    def test_pchannel_gateway_timeout_uses_connect_timeout(self):
        """_pchannel sets chanworker timeout from task connect_timeout"""
        task = task_self()

        # Build a minimal metaworker stand-in: _pchannel reads only
        # metaworker.invoke_gateway (a command string); the object must also
        # be hashable because _pchannel stores it in a set.
        class _FakeMetaWorker:
            invoke_gateway = "python3 -m ClusterShell.Gateway -Bu"
        metaworker = _FakeMetaWorker()

        # Patch task.schedule so we capture the chanworker without starting
        # any engine or SSH process.  Save and restore so other tests are
        # not affected (task_self() is a thread-singleton that persists).
        captured = {}
        original_schedule = task.schedule

        def _fake_schedule(worker):
            captured["worker"] = worker

        task.schedule = _fake_schedule
        try:
            # Case 1: connect_timeout > 0 → all underlying clients should have
            # fire_delay matching connect_timeout (EngineBaseTimer stores timeout
            # as fire_delay; it comes from EngineClient.__init__).
            task._info["connect_timeout"] = 7
            task._pchannel("dummy-gw-a", metaworker)
            worker = captured.get("worker")
            self.assertIsNotNone(worker, "_pchannel did not call schedule")
            self.assertTrue(hasattr(worker, "_clients") and worker._clients,
                            "chanworker has no _clients list")
            client = worker._clients[0]
            self.assertAlmostEqual(client.fire_delay, 7,
                                   msg="chanworker client fire_delay should equal connect_timeout")

            # Reset gateways so a new channel is created for the next sub-case.
            task.gateways = {}

            # Case 2: connect_timeout == 0 → no timer, fire_delay should be -1
            # (EngineBaseTimer.__init__ maps None → -1.0; 0 is mapped to None by
            # _pchannel because 0 means "unlimited" in ClusterShell convention).
            task._info["connect_timeout"] = 0
            task._pchannel("dummy-gw-b", metaworker)
            worker = captured.get("worker")
            self.assertIsNotNone(worker)
            self.assertTrue(hasattr(worker, "_clients") and worker._clients)
            client = worker._clients[0]
            self.assertLess(client.fire_delay, 0,
                            "chanworker fire_delay should be -1 (no timer) when connect_timeout=0")
        finally:
            task.schedule = original_schedule
