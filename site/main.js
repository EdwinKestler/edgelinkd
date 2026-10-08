// n2link project site: copy buttons, config tabs, screenshot viewer, mobile menu. No dependencies.
(function () {
  "use strict";

  function copyText(text) {
    if (navigator.clipboard && window.isSecureContext) {
      return navigator.clipboard.writeText(text);
    }
    return new Promise(function (resolve, reject) {
      var area = document.createElement("textarea");
      area.value = text;
      area.setAttribute("readonly", "");
      area.style.position = "fixed";
      area.style.opacity = "0";
      document.body.appendChild(area);
      area.select();
      try { document.execCommand("copy") ? resolve() : reject(new Error("copy failed")); }
      catch (err) { reject(err); }
      finally { document.body.removeChild(area); }
    });
  }

  function flash(button, ok) {
    var use = button.querySelector("use");
    var label = button.querySelector("span");
    var oldLabel = label ? label.textContent : null;
    if (use) use.setAttribute("href", ok ? "#i-check" : "#i-x");
    if (label) label.textContent = ok ? "Copied" : "Press Ctrl+C";
    button.classList.add("done");
    setTimeout(function () {
      if (use) use.setAttribute("href", "#i-copy");
      if (label) label.textContent = oldLabel;
      button.classList.remove("done");
    }, 1600);
  }

  document.addEventListener("click", function (event) {
    var button = event.target.closest("[data-copy], [data-copy-target]");
    if (!button) return;
    var text = button.getAttribute("data-copy");
    if (text === null) {
      var target = document.getElementById(button.getAttribute("data-copy-target"));
      text = target ? target.textContent : "";
    }
    copyText(text.trim()).then(function () { flash(button, true); }, function () { flash(button, false); });
  });

  // Config tabs (WAI-ARIA tabs pattern: arrows, Home, End).
  var tabs = Array.prototype.slice.call(document.querySelectorAll('[role="tab"]'));
  function selectTab(tab, focus) {
    tabs.forEach(function (t) {
      var selected = t === tab;
      t.setAttribute("aria-selected", String(selected));
      t.tabIndex = selected ? 0 : -1;
      var panel = document.getElementById(t.getAttribute("aria-controls"));
      if (panel) panel.hidden = !selected;
    });
    if (focus) tab.focus();
  }
  tabs.forEach(function (tab, index) {
    tab.addEventListener("click", function () { selectTab(tab, false); });
    tab.addEventListener("keydown", function (event) {
      var next = null;
      if (event.key === "ArrowRight") next = tabs[(index + 1) % tabs.length];
      if (event.key === "ArrowLeft") next = tabs[(index - 1 + tabs.length) % tabs.length];
      if (event.key === "Home") next = tabs[0];
      if (event.key === "End") next = tabs[tabs.length - 1];
      if (next) { event.preventDefault(); selectTab(next, true); }
    });
  });

  // Screenshot viewer.
  var dialog = document.getElementById("lightbox");
  var dialogImg = document.getElementById("lightbox-img");
  var dialogCaption = document.getElementById("lightbox-caption");
  function openLightbox(src, caption, alt) {
    if (!dialog || typeof dialog.showModal !== "function") { window.open(src, "_blank", "noopener"); return; }
    dialogImg.src = src;
    dialogImg.alt = alt || caption || "";
    dialogCaption.textContent = caption || "";
    dialog.showModal();
  }
  document.addEventListener("click", function (event) {
    var trigger = event.target.closest("[data-lightbox-src]");
    if (trigger) {
      var img = trigger.querySelector("img");
      openLightbox(trigger.getAttribute("data-lightbox-src"), trigger.getAttribute("data-caption"), img && img.alt);
      return;
    }
    var heroImg = event.target.closest("img[data-lightbox]");
    if (heroImg) openLightbox(heroImg.currentSrc || heroImg.src, heroImg.getAttribute("data-lightbox"), heroImg.alt);
  });
  if (dialog) {
    dialog.addEventListener("click", function (event) {
      if (event.target === dialog || event.target.closest("[data-close]")) dialog.close();
    });
  }

  // Mobile menu.
  var menuButton = document.querySelector(".menu-btn");
  var links = document.getElementById("nav-links");
  if (menuButton && links) {
    menuButton.addEventListener("click", function () {
      var open = links.classList.toggle("open");
      menuButton.setAttribute("aria-expanded", String(open));
      menuButton.setAttribute("aria-label", open ? "Close menu" : "Open menu");
      menuButton.querySelector("use").setAttribute("href", open ? "#i-x" : "#i-menu");
    });
    links.addEventListener("click", function (event) {
      if (event.target.closest("a")) { links.classList.remove("open"); menuButton.setAttribute("aria-expanded", "false"); menuButton.querySelector("use").setAttribute("href", "#i-menu"); }
    });
  }
})();
