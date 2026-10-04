"""Root conftest for the repository.

`harness/scorecard_guard.py` is registered from here rather than from
`tests/conftest.py` because `harness/sync_upstream_tests.sh` rsyncs `tests/`
from upstream with `--delete` on every scorecard run — anything we put there is
erased before pytest starts. A root-level conftest is outside the synced tree,
and pytest loads it for every test under the rootdir.
"""

pytest_plugins = ("harness.scorecard_guard",)
