(() => {
  const text = (el) => {
    if (!el) return null;
    const copy = el.cloneNode(true);
    copy.querySelectorAll('script,style,noscript').forEach((n) => n.remove());
    const t = copy.textContent.replace(/[\u200e\u200f]/g, '').replace(/\s+/g, ' ').trim();
    return t || null;
  };
  const cards = [...document.querySelectorAll('div[data-component-type="s-search-result"][data-asin]')];
  const results = cards.filter((c) => c.dataset.asin).slice(0, 80).map((c) => {
    // Newer cards put the brand in a first, one-line h2 and the name in the
    // last; badges such as sustainability labels add h2s outside the title.
    const headings = [...(c.querySelector('[data-cy="title-recipe"]') || c).querySelectorAll('h2')];
    const name = headings[headings.length - 1];
    const brand = headings.length > 1 ? headings[0] : null;
    return {
      asin: c.dataset.asin,
      title: (name && name.getAttribute('aria-label')) || text(name),
      brand: text(brand),
      price: text(c.querySelector('.a-price:not(.a-text-price) .a-offscreen')),
      listPrice: text(c.querySelector('.a-price.a-text-price[data-a-strike="true"] .a-offscreen')),
      rating: text(c.querySelector('.a-icon-alt')),
      reviewCount: text(c.querySelector('a[href*="#customerReviews"] span, [data-csa-c-content-id*="ratings-count"] span, span.s-underline-text')),
      sponsored: !!c.querySelector('.puis-sponsored-label-text, [aria-label*="Sponsored"]'),
      delivery: text(c.querySelector('[data-cy="delivery-recipe"]')),
      badge: text(c.querySelector('.a-badge-text')),
    };
  });
  const total = text(document.querySelector('[data-component-type="s-result-info-bar"] h2 span, .s-desktop-toolbar h2 span'));
  return JSON.stringify({ total, results });
})()
