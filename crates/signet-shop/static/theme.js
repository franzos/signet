// Appearance control (System/Light/Dark). The server renders the initial
// state from the `license_theme` cookie, so there is no first-paint flash and
// System is handled purely by CSS `prefers-color-scheme`. This just handles
// clicks: write the cookie, flip `data-theme`, and update the segmented radios.
(function () {
  'use strict';
  var COOKIE = 'license_theme';
  var group = document.querySelector('[data-theme-toggle]');
  if (!group) return;
  var btns = Array.prototype.slice.call(group.querySelectorAll('[data-theme-value]'));

  function apply(pref) {
    document.documentElement.setAttribute('data-theme', pref);
    btns.forEach(function (b) {
      var on = b.getAttribute('data-theme-value') === pref;
      b.setAttribute('aria-checked', on ? 'true' : 'false');
      b.tabIndex = on ? 0 : -1;
    });
  }

  function set(pref) {
    var secure = location.protocol === 'https:' ? '; Secure' : '';
    document.cookie = COOKIE + '=' + pref +
      '; Path=/; Max-Age=31536000; SameSite=Lax' + secure;
    apply(pref);
  }

  btns.forEach(function (b, i) {
    b.addEventListener('click', function () { set(b.getAttribute('data-theme-value')); });
    // Radiogroup arrow-key navigation.
    b.addEventListener('keydown', function (e) {
      if (e.key !== 'ArrowRight' && e.key !== 'ArrowLeft' &&
          e.key !== 'ArrowDown' && e.key !== 'ArrowUp') return;
      e.preventDefault();
      var fwd = e.key === 'ArrowRight' || e.key === 'ArrowDown';
      var next = btns[(i + (fwd ? 1 : btns.length - 1)) % btns.length];
      next.focus();
      set(next.getAttribute('data-theme-value'));
    });
  });
})();
