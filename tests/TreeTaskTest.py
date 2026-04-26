"""
Unit test for ClusterShell.Task in tree mode
"""

import logging
import os
from textwrap import dedent
import unittest

from ClusterShell.NodeSet import NodeSet
from ClusterShell.Propagation import PropagationTreeRouter, RouteResolvingError
from ClusterShell.Task import task_self
from ClusterShell.Topology import TopologyError, TopologyParser

from TLib import HOSTNAME, make_temp_file

# live logging with nosetests --nologcapture
logging.basicConfig(level=logging.DEBUG)


class TreeTaskTest(unittest.TestCase):
    """Test cases for Tree-related Task methods"""

    def tearDown(self):
        """clear task topology"""
        task_self().topology = None

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


class PropagationRouterTest(unittest.TestCase):
    """Unit tests for PropagationTreeRouter._best_next_hop load balancing"""

    def _make_router(self, topology_str, root):
        """Build a PropagationTreeRouter from a topology config string."""
        topofile = make_temp_file(topology_str.encode())
        parser = TopologyParser(topofile.name)
        topology = parser.tree(root)
        return PropagationTreeRouter(root, topology), topofile

    def test_best_next_hop_selects_least_loaded(self):
        """_best_next_hop must return the gateway with fewest connections"""
        # Topology: admin -> gw[0-1] -> leaf[0-9]
        # gw0 carries nodes leaf[0-4], gw1 carries nodes leaf[5-9]
        topo = dedent("""
            [Main]
            admin: gw[0-1]
            gw0: leaf[0-4]
            gw1: leaf[5-9]
        """)
        router, _f = self._make_router(topo, "admin")

        # Simulate gw0 already handling 3 connections, gw1 handling 1
        router.nodes_fanin["gw0"] = 3
        router.nodes_fanin["gw1"] = 1

        # next_hop for a leaf behind gw0 should still resolve via the routing
        # table, but the load data is used only when multiple gateways serve
        # the same network; here each gateway owns a distinct leaf set so the
        # table routes deterministically.  Test _best_next_hop directly.
        candidates = NodeSet("gw[0-1]")
        best = router._best_next_hop(candidates)
        self.assertEqual(str(best), "gw1",
                         "_best_next_hop must pick the gateway with fewer connections")

    def test_best_next_hop_excludes_unreachable(self):
        """_best_next_hop must ignore gateways marked unreachable"""
        topo = dedent("""
            [Main]
            admin: gw[0-1]
            gw0: leaf[0-4]
            gw1: leaf[5-9]
        """)
        router, _f = self._make_router(topo, "admin")

        # gw0 is unreachable; only gw1 is a valid candidate
        router.mark_unreachable("gw0")
        candidates = NodeSet("gw[0-1]")
        best = router._best_next_hop(candidates)
        self.assertEqual(str(best), "gw1",
                         "_best_next_hop must skip unreachable gateways")

    def test_best_next_hop_all_unreachable_returns_none(self):
        """_best_next_hop returns None when every candidate is unreachable"""
        topo = dedent("""
            [Main]
            admin: gw[0-1]
            gw0: leaf[0-4]
            gw1: leaf[5-9]
        """)
        router, _f = self._make_router(topo, "admin")

        router.mark_unreachable("gw0")
        router.mark_unreachable("gw1")
        candidates = NodeSet("gw[0-1]")
        best = router._best_next_hop(candidates)
        self.assertIsNone(best,
                          "_best_next_hop must return None when all candidates are unreachable")
