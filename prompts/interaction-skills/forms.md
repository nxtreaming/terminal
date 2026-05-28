# Forms

Treat real user-facing forms as browser-input workflows, not DOM mutation
workflows. Modern forms often keep framework state separate from DOM attributes,
so assigning values in JavaScript can make the page look filled while React,
Vue, Rails UJS, validation, or CAPTCHA state still believes it is empty.

Default flow:

1. Screenshot the form before filling when visual state matters.
2. Click the visible field/control with `click_at_xy(...)` whenever you can.
3. Enter text with `type_text(...)` or `press_key(...)`. Use
   `fill_input(selector, text, timeout=...)` only when a stable visible selector
   is clearly the best handle; it should behave like focus/click plus browser
   input, not DOM mutation.
4. Use visible clicks for checkboxes, radios, buttons, and custom controls. Use
   the dropdown skill for selects, comboboxes, and menus.
5. Use read-only JS only when screenshots are insufficient to identify labels,
   current values, or stable selectors.
6. Screenshot after meaningful fills, checkbox changes, validation changes, and
   before final submit.

Do not do these on real forms unless the user explicitly asks for low-level
debugging:

- assigning `element.value`, `element.checked`, `selectedIndex`, or similar
  state directly
- dispatching synthetic `input`, `change`, `click`, `keydown`, or `keyup`
  events from page JavaScript to make the app accept a value
- calling framework-private setters such as React value trackers or fibers
- installing `MutationObserver` loops that reapply form values after renders
- bulk DOM scripts that fill many fields at once

The rule is: JS may inspect forms; browser input actions mutate forms.
