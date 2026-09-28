"""ClusterShell — dual-backend package for consortium migration.

Backend selection via CONSORTIUM_BACKEND environment variable:
  - "rust"   (default): Rust-backed where a binding exists (PyO3 _consortium),
                      original pure-Python from lib/ClusterShell/ everywhere else
  - "python":           Original pure-Python from lib/ClusterShell/ throughout

The Rust backend is deliberately *hybrid*, not a wholesale replacement. This
package ships a binding only for the modules that have a Rust implementation —
currently RangeSet and NodeSet, and nothing else. Every other module
(Task, Event, Topology, Defaults, NodeUtils, Propagation, Communication,
MsgTree, Gateway, and the whole CLI/, Engine/, Worker/ trees) is unported, so
the upstream pure-Python source at lib/ClusterShell/ stays reachable through
this package's __path__. It is appended, never prepended, so a binding always
wins over upstream and upstream only fills the gaps.

Do not add a shim for an unported module. A shim shadows the upstream module it
claims to re-export, and re-exporting it faithfully is what makes the hybrid
layer hard to get right: a shim that is merely *close* fails with a confusing
ImportError from inside unrelated upstream code, which is exactly how
ClusterShell.CLI came to be reported as missing for every test module.
TEST_MAPPING.toml is the inventory of what is actually ported.

When backend is "python", we rewrite sys.path so that imports resolve
to lib/ClusterShell/ (the upstream pure-Python implementation) wholesale.
"""

import os
import sys
import warnings

__version__ = "0.1.0"

BACKEND = os.environ.get("CONSORTIUM_BACKEND", "rust")


def _resolve_lib_dir(this_dir):
    """Locate the upstream lib/ directory holding the pure-Python ClusterShell.

    Walk up from the bindings package looking for lib/ClusterShell rather than
    counting "..": the bindings tree is nested a variable depth below the repo
    root, and a hardcoded count silently resolves one level too high.
    """
    override = os.environ.get("LIB_CLUSTERSHELL")
    if override and os.path.isdir(os.path.join(override, "ClusterShell")):
        return override
    cur = os.path.abspath(this_dir)
    while True:
        candidate = os.path.join(cur, "lib")
        if os.path.isdir(os.path.join(candidate, "ClusterShell")):
            return candidate
        parent = os.path.dirname(cur)
        if parent == cur:
            break
        cur = parent
    return override if override else os.path.normpath(
        os.path.join(this_dir, "..", "..", "..", "lib")
    )


if BACKEND == "python":
    # Redirect all ClusterShell imports to the original pure-Python source.
    # We do this by inserting lib/ at the front of sys.path and removing
    # this package's directory so Python resolves to the original.
    _this_dir = os.path.dirname(os.path.abspath(__file__))
    _lib_dir = _resolve_lib_dir(_this_dir)

    if os.path.isdir(_lib_dir):
        # Remove this package's parent from sys.path if present
        _parent = os.path.dirname(_this_dir)
        if _parent in sys.path:
            sys.path.remove(_parent)

        # Add lib/ to front
        if _lib_dir not in sys.path:
            sys.path.insert(0, _lib_dir)

        # Remove ourselves from sys.modules so the next import of
        # ClusterShell resolves to the original in lib/
        for key in list(sys.modules.keys()):
            if key == "ClusterShell" or key.startswith("ClusterShell."):
                del sys.modules[key]

        # Re-import from the original
        import importlib
        _orig = importlib.import_module("ClusterShell")
        # Copy its namespace into ours
        globals().update(_orig.__dict__)
    else:
        raise RuntimeError(
            f"CONSORTIUM_BACKEND=python but cannot find original ClusterShell at {_lib_dir}. "
            f"Set LIB_CLUSTERSHELL env var to the lib/ directory containing the original."
        )
else:
    # Rust backend, hybrid. Keep the upstream lib/ClusterShell/ on this
    # package's __path__ (appended, never prepended) so every submodule that
    # has no Rust binding yet — CLI/, Engine/, Worker/ — still imports, exactly
    # as the original-backend step does. Without this, `import ClusterShell.CLI`
    # raises ModuleNotFoundError at collection and the whole step dies before a
    # single test is imported.
    _this_dir = os.path.dirname(os.path.abspath(__file__))
    _lib_dir = _resolve_lib_dir(_this_dir)
    _lib_pkg = os.path.join(_lib_dir, "ClusterShell")
    if os.path.isdir(_lib_pkg):
        if _lib_pkg not in __path__:
            __path__.append(_lib_pkg)
    else:
        # Say so loudly: the resulting ModuleNotFoundError would otherwise name
        # only the missing submodule and hide the real cause.
        warnings.warn(
            f"CONSORTIUM_BACKEND=rust but no upstream ClusterShell found at "
            f"{_lib_dir}. Modules without a Rust binding (ClusterShell.CLI, "
            f"ClusterShell.Engine, ClusterShell.Worker) will fail to import. "
            f"Set LIB_CLUSTERSHELL to the lib/ directory containing the original.",
            RuntimeWarning,
            stacklevel=2,
        )
