# SPDX-FileCopyrightText: 2026 Sascha Brawer <sascha@brawer.ch>
# SPDX-License-Identifier: MIT

# cloud_test.py is a standalone script, not a package -- puts its
# directory on sys.path so test_cloud_test.py can `import cloud_test`
# directly, the same way it'd be run from the command line.
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
