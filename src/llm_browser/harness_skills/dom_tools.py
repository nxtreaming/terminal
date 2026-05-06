from __future__ import annotations

import json
import time
from typing import Any, Dict, Optional

from llm_browser.harness.api import HelperAPI


SKILL = {
    "name": "dom_tools",
    "description": "Generic DOM helpers for shadow-DOM text, text clicks, and element screenshots.",
    "exports": ["deep_text", "click_text", "screenshot_element"],
}


def install(api: HelperAPI) -> Dict[str, Any]:
    runtime = api.runtime

    def deep_text(max_chars: int = 12000) -> str:
        script = f"""
(() => {{
  const maxChars = {int(max_chars)};
  const roots = [];
  const seenRoots = new Set();
  const seenLines = new Set();
  const lines = [];

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

  function isHiddenElement(element) {{
    if (!element || element.nodeType !== Node.ELEMENT_NODE) return false;
    const tag = element.tagName;
    if (["SCRIPT", "STYLE", "NOSCRIPT", "TEMPLATE", "SVG"].includes(tag)) return true;
    const style = getComputedStyle(element);
    return style.display === "none" || style.visibility === "hidden";
  }}

  function addText(value) {{
    const normalized = String(value || "").replace(/\\s+/g, " ").trim();
    if (!normalized || seenLines.has(normalized)) return;
    seenLines.add(normalized);
    lines.push(normalized);
  }}

  function walk(node) {{
    if (!node) return;
    if (node.nodeType === Node.TEXT_NODE) {{
      addText(node.nodeValue);
      return;
    }}
    if (node.nodeType !== Node.ELEMENT_NODE && node.nodeType !== Node.DOCUMENT_FRAGMENT_NODE) return;
    if (node.nodeType === Node.ELEMENT_NODE && isHiddenElement(node)) return;
    if (node.shadowRoot) walk(node.shadowRoot);
    for (const child of Array.from(node.childNodes || [])) walk(child);
  }}

  addRoot(document.body || document.documentElement);
  for (const root of roots) walk(root);
  return lines.join("\\n").slice(0, maxChars);
}})()
"""
        return str(runtime.js(script, await_promise=True, repl_mode=False) or "")

    def click_text(
        text_or_pattern: str,
        timeout_s: float = 5.0,
        regex: bool = False,
        exact: bool = False,
        case_sensitive: bool = False,
    ) -> Dict[str, Any]:
        deadline = time.monotonic() + timeout_s
        last_result: Dict[str, Any] = {"clicked": False, "matches": []}
        while True:
            api.check_cancel()
            result = runtime.js(
                _click_text_script(
                    text_or_pattern,
                    regex=regex,
                    exact=exact,
                    case_sensitive=case_sensitive,
                ),
                await_promise=True,
                repl_mode=False,
                user_gesture=True,
            )
            if isinstance(result, dict):
                last_result = result
                if result.get("clicked"):
                    return result
            if time.monotonic() >= deadline:
                return last_result
            api.sleep(0.25)

    def screenshot_element(
        selector: str,
        label: Optional[str] = None,
        attach: bool = True,
        padding: float = 8.0,
        timeout_s: float = 8.0,
        timeout: Optional[float] = None,
    ) -> Dict[str, Any]:
        if timeout is not None:
            timeout_s = timeout
        selector_json = json.dumps(selector)
        rect = runtime.js(
            f"""
            (() => {{
              const el = document.querySelector({selector_json});
              if (!el) return null;
              const r = el.getBoundingClientRect();
              return {{
                x: r.left + scrollX,
                y: r.top + scrollY,
                width: r.width,
                height: r.height,
                viewportX: r.left,
                viewportY: r.top,
                tag: el.tagName,
                text: (el.innerText || el.alt || el.getAttribute('aria-label') || '').trim().slice(0, 300)
              }};
            }})()
            """,
            await_promise=False,
        )
        if not isinstance(rect, dict):
            raise ValueError(f"element not found for selector: {selector}")
        pad = max(0.0, float(padding))
        clip = {
            "x": max(0.0, float(rect.get("x") or 0) - pad),
            "y": max(0.0, float(rect.get("y") or 0) - pad),
            "width": max(1.0, float(rect.get("width") or 1) + pad * 2),
            "height": max(1.0, float(rect.get("height") or 1) + pad * 2),
            "scale": 1.0,
        }
        image = runtime.screenshot(
            label=label or f"element_{selector}",
            attach=attach,
            full_page=False,
            timeout_s=timeout_s,
            clip=clip,
        )
        if attach:
            api.emit_image(image)
        return {"selector": selector, "rect": rect, "clip": clip, "image": image.to_dict()}

    return {
        "deep_text": deep_text,
        "click_text": click_text,
        "screenshot_element": screenshot_element,
    }


def _click_text_script(
    text_or_pattern: str,
    *,
    regex: bool,
    exact: bool,
    case_sensitive: bool,
) -> str:
    needle = json.dumps(text_or_pattern)
    return f"""
(() => {{
  const needle = {needle};
  const useRegex = {json.dumps(regex)};
  const exact = {json.dumps(exact)};
  const caseSensitive = {json.dumps(case_sensitive)};
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

  function normalize(value) {{
    return String(value || "").replace(/\\s+/g, " ").trim();
  }}

  let compiled = null;
  if (useRegex) {{
    try {{
      compiled = new RegExp(needle, caseSensitive ? "" : "i");
    }} catch (error) {{
      return {{clicked: false, error: String(error), matches: []}};
    }}
  }}

  function isMatch(value) {{
    const text = normalize(value);
    if (!text) return false;
    if (compiled) return compiled.test(text);
    if (exact) return caseSensitive ? text === needle : text.toLowerCase() === needle.toLowerCase();
    return caseSensitive ? text.includes(needle) : text.toLowerCase().includes(needle.toLowerCase());
  }}

  function candidateText(element) {{
    const values = [
      element.innerText,
      element.textContent,
      element.value,
      element.getAttribute && element.getAttribute("aria-label"),
      element.getAttribute && element.getAttribute("title"),
      element.getAttribute && element.getAttribute("alt"),
      element.getAttribute && element.getAttribute("data-testid"),
      element.id,
    ];
    return normalize(values.filter(Boolean).join(" "));
  }}

  function isVisible(element) {{
    if (!element || element.nodeType !== Node.ELEMENT_NODE) return false;
    const style = getComputedStyle(element);
    if (style.display === "none" || style.visibility === "hidden" || style.pointerEvents === "none") return false;
    const rect = element.getBoundingClientRect();
    return rect.width > 0 && rect.height > 0;
  }}

  function isClickable(element) {{
    if (!element || element.nodeType !== Node.ELEMENT_NODE) return false;
    const tag = element.tagName;
    const role = (element.getAttribute("role") || "").toLowerCase();
    const className = String(element.className || "").toLowerCase();
    return (
      ["BUTTON", "A", "INPUT", "TEXTAREA", "SELECT", "SUMMARY", "LABEL"].includes(tag) ||
      role === "button" ||
      role === "link" ||
      element.hasAttribute("onclick") ||
      element.hasAttribute("tabindex") ||
      /\\b(btn|button|accept|agree|consent|cookie|save|reject|decline)\\b/.test(className)
    );
  }}

  function clickableTarget(element) {{
    let current = element;
    for (let i = 0; current && i < 8; i += 1) {{
      if (isClickable(current) && isVisible(current)) return current;
      current = current.parentElement || (current.getRootNode && current.getRootNode().host) || null;
    }}
    return isVisible(element) ? element : null;
  }}

  function describe(element, text) {{
    const rect = element.getBoundingClientRect();
    return {{
      tag: element.tagName,
      id: element.id || "",
      className: String(element.className || ""),
      role: element.getAttribute("role") || "",
      text: normalize(text).slice(0, 240),
      x: Math.round(rect.left + rect.width / 2),
      y: Math.round(rect.top + rect.height / 2),
    }};
  }}

  function clickElement(element) {{
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
  }}

  addRoot(document.body || document.documentElement);
  const matches = [];
  const seenElements = new Set();
  for (const root of roots) {{
    let elements = [];
    try {{
      elements = Array.from(root.querySelectorAll(
        "button,a,input,textarea,select,summary,label,[role='button'],[role='link'],[onclick],[tabindex],.btn,.button"
      ));
    }} catch (_) {{}}
    for (const element of elements) {{
      if (seenElements.has(element)) continue;
      seenElements.add(element);
      const text = candidateText(element);
      if (!isMatch(text)) continue;
      const target = clickableTarget(element);
      if (!target) continue;
      const item = describe(target, text);
      matches.push(item);
      clickElement(target);
      return {{clicked: true, ...item, matches: matches.slice(0, 10)}};
    }}
  }}
  return {{clicked: false, matches: matches.slice(0, 10)}};
}})()
"""
