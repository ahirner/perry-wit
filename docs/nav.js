// Link the handbook's menu bar to the rest of the site; the book is published under `docs/`.
(() => {
  const bar = document.querySelector('.right-buttons');
  if (!bar) return;
  const nav = document.createElement('nav');
  nav.className = 'pw-site-nav';
  nav.setAttribute('aria-label', 'Site');
  for (const [label, href] of [['Home', '../index.html'], ['Playground', '../play/']]) {
    const link = document.createElement('a');
    link.textContent = label;
    link.href = path_to_root + href;
    nav.append(link);
  }
  bar.prepend(nav);
})();
