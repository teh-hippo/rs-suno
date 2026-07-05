// Reveal only the selected browser's token-capture steps.
// Progressive enhancement: if this script does not run, the `.js` class is
// never added and every browser's steps remain visible.
(function () {
  "use strict";

  var picker = document.getElementById("cookie-capture");
  if (!picker) {
    return;
  }

  var tabs = picker.querySelectorAll(".browser-picker__buttons [data-browser]");
  var steps = picker.querySelectorAll(".browser-steps");
  if (tabs.length === 0 || steps.length === 0) {
    return;
  }

  function select(browser) {
    tabs.forEach(function (tab) {
      tab.setAttribute(
        "aria-selected",
        tab.dataset.browser === browser ? "true" : "false"
      );
    });
    steps.forEach(function (step) {
      step.hidden = step.dataset.browser !== browser;
    });
  }

  picker.classList.add("js");
  tabs.forEach(function (tab) {
    tab.addEventListener("click", function () {
      select(tab.dataset.browser);
    });
  });
  select(tabs[0].dataset.browser);
})();
