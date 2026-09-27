"""ClusterShell.NodeSet — backend-aware shim.

When CONSORTIUM_BACKEND=rust (default), imports from Rust PyO3 bindings.
When CONSORTIUM_BACKEND=python, this file is never reached (the __init__.py
redirects the entire ClusterShell package to the original pure-Python source).
"""

from ClusterShell._consortium import (  # noqa: F401
    NodeSet,
    NodeSetError,
    NodeSetException,
    NodeSetExternalError,
    NodeSetParseError,
    RangeSetParseError,
)
# Upstream's NodeSet.py re-exports these; they are Rust types too, so this is a
# faithful mirror rather than a mix of Rust and pure-Python objects.
from ClusterShell.RangeSet import AUTOSTEP_DISABLED, RangeSet  # noqa: F401
