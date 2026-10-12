(function () {
  if (!/(?:^|; )hideFirefoxWarning=on(?:;|$)/.test(document.cookie)) return;
  var warning = document.querySelector(".firefox-media-warning");
  if (warning) warning.remove();
  var form = document.getElementById("ignore-firefox-warning");
  if (form) form.remove();
})();
