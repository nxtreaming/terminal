"""Editable helpers for this browser use terminal session.

Keep task-specific browser routines here. This file is loaded into the
persistent Python browser namespace by reload_agent_helpers().
"""

from browser_helpers import *


def browser_state():
    return {
        "page": page_info(),
        "tabs": list_tabs(),
    }
