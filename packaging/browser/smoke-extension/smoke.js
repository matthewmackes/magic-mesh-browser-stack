(function () {
  var markerId = "mde-cef-extension-smoke-marker";
  if (document.getElementById(markerId)) {
    return;
  }
  var autofillOk = false;
  var inputs = Array.prototype.slice.call(document.querySelectorAll("input"));
  var username = inputs.find(function (input) {
    var type = (input.getAttribute("type") || "text").toLowerCase();
    var hint = [
      input.getAttribute("name") || "",
      input.getAttribute("id") || "",
      input.getAttribute("autocomplete") || ""
    ].join(" ").toLowerCase();
    return (type === "text" || type === "email") &&
      (hint.indexOf("user") !== -1 || hint.indexOf("email") !== -1 || hint.indexOf("login") !== -1);
  }) || inputs.find(function (input) {
    var type = (input.getAttribute("type") || "text").toLowerCase();
    return type === "text" || type === "email";
  });
  var password = inputs.find(function (input) {
    return (input.getAttribute("type") || "").toLowerCase() === "password";
  });

  function setField(input, value) {
    input.focus();
    input.value = value;
    input.setAttribute("data-mde-cef-extension-autofilled", "true");
    input.dispatchEvent(new Event("input", { bubbles: true }));
    input.dispatchEvent(new Event("change", { bubbles: true }));
  }

  if (username && password) {
    setField(username, "mde-cef-extension-smoke-user");
    setField(password, "mde-cef-extension-smoke-pass");
    autofillOk = username.value === "mde-cef-extension-smoke-user" &&
      password.value === "mde-cef-extension-smoke-pass";
  }

  var marker = document.createElement("div");
  marker.id = markerId;
  marker.textContent = autofillOk
    ? "mde-cef-extension-smoke-ok mde-cef-extension-autofill-ok"
    : "mde-cef-extension-smoke-ok";
  marker.setAttribute("data-mde-cef-extension-smoke", "ok");
  if (autofillOk) {
    marker.setAttribute("data-mde-cef-extension-autofill", "ok");
  }
  marker.style.cssText = [
    "position:fixed",
    "left:0",
    "bottom:0",
    "z-index:2147483647",
    "font:12px sans-serif",
    "padding:2px 4px",
    "background:#111",
    "color:#fff",
    "pointer-events:none"
  ].join(";");
  (document.body || document.documentElement).appendChild(marker);

  try {
    var beacon = document.createElement("img");
    beacon.alt = "";
    beacon.width = 1;
    beacon.height = 1;
    beacon.style.cssText = "position:absolute;left:-9999px;top:-9999px;width:1px;height:1px";
    beacon.src = "/mde-cef-extension-smoke?marker=ok&autofill=" + (autofillOk ? "ok" : "missing");
    (document.body || document.documentElement).appendChild(beacon);
  } catch (err) {
    // The visible marker remains the primary in-page signal; the localhost beacon
    // is an extra live-runner proof when the smoke page is served locally.
  }
}());
