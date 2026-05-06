from __future__ import annotations

import json
import time
from typing import Any, Dict

from llm_browser.harness.api import HelperAPI
from llm_browser.harness_skills.dom_tools import install as install_dom_tools


SKILL = {
    "name": "cookie_banners",
    "description": "Cookie-consent banner recipes, including common vendor selectors and text fallbacks.",
    "exports": ["dismiss_cookie_banners"],
}


def install(api: HelperAPI) -> Dict[str, Any]:
    runtime = api.runtime
    click_text = api.namespace.get("click_text")
    if not callable(click_text):
        click_text = install_dom_tools(api)["click_text"]

    def dismiss_cookie_banners(timeout_s: float = 5.0, prefer: str = "accept") -> Dict[str, Any]:
        deadline = time.monotonic() + timeout_s
        vendor_result = runtime.js(_dismiss_cookie_vendor_script(prefer), await_promise=True, repl_mode=False, user_gesture=True)
        if isinstance(vendor_result, dict) and vendor_result.get("clicked"):
            return vendor_result

        accept_patterns = [
            r"^accept all$",
            r"^accept cookies?$",
            r"^allow all$",
            r"^agree$",
            r"^i agree$",
            r"^got it$",
            r"^ok$",
            r"^continue$",
            r"^save settings$",
        ]
        reject_patterns = [
            r"^reject all$",
            r"^decline$",
            r"^necessary only$",
            r"^essential only$",
        ]
        patterns = accept_patterns if prefer != "reject" else reject_patterns + accept_patterns
        last_result: Dict[str, Any] = {"clicked": False, "matches": []}
        for pattern in patterns:
            api.check_cancel()
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                break
            result = click_text(pattern, timeout_s=min(remaining, 1.25), regex=True, case_sensitive=False)
            last_result = result
            if result.get("clicked"):
                result["kind"] = "cookie-banner"
                result["pattern"] = pattern
                return result
        return last_result

    return {"dismiss_cookie_banners": dismiss_cookie_banners}


def _dismiss_cookie_vendor_script(prefer: str) -> str:
    prefer_reject = prefer == "reject"
    return f"""
(() => {{
  const preferReject = {json.dumps(prefer_reject)};
  const roots = [];
  const seenRoots = new Set();

  function addRoot(root) {{
    if (!root || seenRoots.has(root)) return;
    seenRoots.add(root);
    roots.push(root);
    let elements = [];
    try {{ elements = Array.from(root.querySelectorAll("*")); }} catch (_) {{}}
    for (const element of elements) {{
      if (element.shadowRoot) addRoot(element.shadowRoot);
    }}
  }}

  function visible(element) {{
    if (!element) return false;
    const style = getComputedStyle(element);
    const rect = element.getBoundingClientRect();
    return style.display !== "none" && style.visibility !== "hidden" && rect.width > 0 && rect.height > 0;
  }}

  function click(element, source) {{
    element.scrollIntoView({{block: "center", inline: "center"}});
    const rect = element.getBoundingClientRect();
    const x = rect.left + rect.width / 2;
    const y = rect.top + rect.height / 2;
    for (const type of ["pointerdown", "mousedown", "pointerup", "mouseup", "click"]) {{
      element.dispatchEvent(new MouseEvent(type, {{
        bubbles: true,
        cancelable: true,
        view: window,
        clientX: x,
        clientY: y,
        button: 0,
      }}));
    }}
    if (typeof element.click === "function") element.click();
    return {{
      clicked: true,
      source,
      tag: element.tagName,
      id: element.id || "",
      className: String(element.className || ""),
      text: String(element.innerText || element.textContent || element.value || "").replace(/\\s+/g, " ").trim().slice(0, 240),
    }};
  }}

  try {{
    if (!preferReject && window.OneTrust && typeof window.OneTrust.AllowAll === "function") {{
      window.OneTrust.AllowAll();
      return {{clicked: true, source: "OneTrust.AllowAll"}};
    }}
  }} catch (_) {{}}
  try {{
    if (window.Cookiebot && window.Cookiebot.dialog) {{
      if (preferReject && typeof window.Cookiebot.submitCustomConsent === "function") {{
        window.Cookiebot.submitCustomConsent(false, false, false);
        return {{clicked: true, source: "Cookiebot.submitCustomConsent"}};
      }}
      if (!preferReject && typeof window.Cookiebot.submitCustomConsent === "function") {{
        window.Cookiebot.submitCustomConsent(true, true, true);
        return {{clicked: true, source: "Cookiebot.submitCustomConsent"}};
      }}
    }}
  }} catch (_) {{}}

  const acceptSelectors = [
    "#onetrust-accept-btn-handler",
    "#accept-recommended-btn-handler",
    ".accept-recommended-btn-handler",
    "[data-testid='uc-accept-all-button']",
    "button[mode='primary']",
    "button[id*='accept' i]",
    "button[class*='accept' i]",
    "a[id*='accept' i]",
    "[role='button'][id*='accept' i]",
  ];
  const rejectSelectors = [
    "#onetrust-reject-all-handler",
    "#reject-recommended-btn-handler",
    "[data-testid='uc-deny-all-button']",
    "button[id*='reject' i]",
    "button[class*='reject' i]",
    "button[id*='decline' i]",
    "button[class*='decline' i]",
  ];
  const selectors = preferReject ? rejectSelectors.concat(acceptSelectors) : acceptSelectors.concat(rejectSelectors);
  addRoot(document.body || document.documentElement);
  for (const root of roots) {{
    for (const selector of selectors) {{
      let elements = [];
      try {{ elements = Array.from(root.querySelectorAll(selector)); }} catch (_) {{}}
      for (const element of elements) {{
        if (visible(element)) return click(element, selector);
      }}
    }}
  }}
  return {{clicked: false}};
}})()
"""
