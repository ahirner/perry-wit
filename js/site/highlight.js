async function initializeHighlighting() {
  try {
    const [{ createHighlighterCore }, { createJavaScriptRegexEngine }, { default: typescript }, { default: wit }, { default: light }, { default: dark }] = await Promise.all([
      import('https://esm.sh/shiki@4.5.0/core'),
      import('https://esm.sh/shiki@4.5.0/engine/javascript'),
      import('https://esm.sh/@shikijs/langs@4.5.0/typescript'),
      import('https://esm.sh/@shikijs/langs@4.5.0/wit'),
      import('https://esm.sh/@shikijs/themes@4.5.0/github-light-high-contrast'),
      import('https://esm.sh/@shikijs/themes@4.5.0/github-dark-high-contrast'),
    ]);
    const highlighter = await createHighlighterCore({
      themes: [light, dark], langs: [typescript, wit], engine: createJavaScriptRegexEngine(),
    });
    function renderCode(code, text, language) {
      const html = highlighter.codeToHtml(text, {
        lang: language,
        themes: { light: 'github-light-high-contrast', dark: 'github-dark-high-contrast' },
        defaultColor: false,
      });
      const highlighted = new DOMParser().parseFromString(html, 'text/html').querySelector('code');
      code.replaceChildren(...highlighted.childNodes);
    }
    for (const input of document.querySelectorAll('textarea[data-language]')) {
      const frame = document.createElement('div');
      frame.className = 'code-editor';
      const layer = document.createElement('pre');
      layer.className = 'code-layer';
      layer.setAttribute('aria-hidden', 'true');
      const code = document.createElement('code');
      layer.append(code);
      input.before(frame);
      frame.append(layer, input);
      const sizeInput = () => {
        input.style.width = `${layer.offsetWidth}px`;
        input.style.height = `${layer.offsetHeight}px`;
      };
      new ResizeObserver(sizeInput).observe(frame);
      let pendingFrame;
      const renderInput = () => {
        cancelAnimationFrame(pendingFrame);
        pendingFrame = requestAnimationFrame(() => {
          try {
            renderCode(code, input.value + '\n', input.dataset.language);
            input.dataset.highlighted = 'true';
            sizeInput();
          } catch (error) {
            input.removeAttribute('data-highlighted');
            console.warn('Editor highlighting unavailable. Showing plain source.', error);
          }
        });
      };
      input.addEventListener('input', renderInput);
      input.addEventListener('editorchange', () => {
        frame.scrollTop = frame.scrollLeft = 0;
        renderInput();
      });
      renderInput();
    }
  } catch (error) {
    console.warn('Syntax highlighting unavailable. Showing plain source.', error);
  }
}

initializeHighlighting();
