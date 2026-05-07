Browser Python quick reference:
- cdp("Domain.method", **params), js(expr), drain_events()
- Target.getTargets / Target.attachToTarget / set_cdp_session(sessionId, target_id=targetId) for tabs and stale sessions
- capture_screenshot(...), click_at_xy(...), press_key(...), type_text(...)
- agent_helpers.py is editable; put task-specific helpers there
