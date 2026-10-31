(() => {
  const title = document.title || '';
  const captchaForm = document.querySelector('form[action*="validateCaptcha"]');
  return JSON.stringify({
    title,
    captcha: !!document.querySelector('#captchacharacters, img[src*="captcha"]'),
    continueButton: !!captchaForm,
    search: !!document.querySelector('.s-main-slot, [data-component-type="s-search-results"]'),
    product: !!document.querySelector('#productTitle'),
    notFound: /^page not found$/i.test(title.trim()),
  });
})()
