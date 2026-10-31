(() => {
  const text = (el) => {
    if (!el) return null;
    const copy = el.cloneNode(true);
    copy.querySelectorAll('script,style,noscript').forEach((n) => n.remove());
    const t = copy.textContent.replace(/[\u200e\u200f]/g, '').replace(/\s+/g, ' ').trim();
    return t || null;
  };
  const q = (selector) => document.querySelector(selector);
  const details = [];
  const tables = '#productDetails_techSpec_section_1 tr, #productDetails_detailBullets_sections1 tr, '
    + '#technicalSpecifications_section_1 tr, #productOverview_feature_div tr';
  for (const row of document.querySelectorAll(tables)) {
    const name = text(row.querySelector('th, td:first-child'));
    const value = text(row.querySelector('td:last-child'));
    if (name && value && name !== value) details.push([name, value]);
  }
  for (const li of document.querySelectorAll('#detailBullets_feature_div li')) {
    const parts = (text(li) || '').split(/\s*:\s*/);
    if (parts.length >= 2) details.push([parts[0], parts.slice(1).join(': ')]);
  }
  const reviews = [...document.querySelectorAll('[data-hook="review"]')].slice(0, 20).map((r) => ({
    title: text(r.querySelector('[data-hook="reviewTitle"], [data-hook="review-title"] span:not(.a-icon-alt):last-child')),
    rating: text(r.querySelector('[data-hook="review-star-rating"], [data-hook="cmps-review-star-rating"]')),
    date: text(r.querySelector('[data-hook="review-date"]')),
    verified: !!r.querySelector('[data-hook="avp-badge"], [data-hook="avp-badge-linkless"]'),
    body: text(r.querySelector('[data-hook="reviewRichContentContainer"], [data-hook="review-body"]')),
  }));
  return JSON.stringify({
    title: text(q('#productTitle')),
    brand: text(q('#bylineInfo')),
    price: text(q('#corePriceDisplay_desktop_feature_div .priceToPay .a-offscreen, #corePrice_feature_div .priceToPay .a-offscreen, '
      + '#corePrice_feature_div .a-offscreen, #corePrice_desktop .a-offscreen, #price_inside_buybox, #tp_price_block_total_price_ww .a-offscreen')),
    listPrice: text(q('#corePriceDisplay_desktop_feature_div .basisPrice .a-offscreen, #corePrice_desktop .a-text-price .a-offscreen')),
    rating: text(q('#acrPopover .a-icon-alt')) || (q('#acrPopover') && q('#acrPopover').title) || null,
    reviewCount: text(q('#acrCustomerReviewText')),
    availability: text(q('#availability > span, #availability')),
    seller: text(q('#merchantInfoFeature_feature_div .offer-display-feature-text-message, #sellerProfileTriggerId')),
    shipsFrom: text(q('#fulfillerInfoFeature_feature_div .offer-display-feature-text-message')),
    bullets: [...document.querySelectorAll('#feature-bullets li, #productFactsDesktopExpander li')].slice(0, 30).map(text).filter(Boolean),
    details: details.slice(0, 80),
    reviews,
    summary: text(q('#product-summary p, [data-hook="cr-insights-widget-summary"] p')),
  });
})()
