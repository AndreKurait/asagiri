// page: setup
window.asagiri.on({ type: 'setup', step: 'choose', existingUbuntu: true,
  existingDistros: ['Ubuntu-First', 'Managed241'], linux: null, sizes: {}, tasks: [], log: [] });
setTimeout(() => {
  const choices = [...document.querySelectorAll('input[name="linux"]')];
  const registered = choices.filter(input => input.value.startsWith('existing:'));
  const managed = registered[1];
  if (managed) {
    managed.click();
    document.querySelector('[data-act="choose"]').click();
  }
  const selected = sent.find(message => message.cmd === 'setup-choose');
  const matches = registered.length === 2 && registered[0].value === 'existing:Ubuntu-First' &&
    selected?.value === 'existing:Managed241';
  const results = [(matches ? 'PASS' : 'FAIL') + ' Setup submits the second registered WSL distro by name'];
  window.asagiri.on({ type: 'setup', step: 'choose', existingUbuntu: true,
    existingDistros: ['Ubuntu-First'], linux: null, sizes: {}, tasks: [], log: [] });
  requestAnimationFrame(() => {
    const button = document.querySelector('[data-act="choose"]');
    button.click();
    const blocked = button.disabled && sent.filter(message => message.cmd === 'setup-choose').length === 1;
    document.querySelector('input[value="existing:Ubuntu-First"]').click();
    button.click();
    const recovered = sent.filter(message => message.cmd === 'setup-choose').at(-1)?.value === 'existing:Ubuntu-First';
    results.push((blocked && recovered ? 'PASS' : 'FAIL') + ' A removed distro requires a new selection');
    report(results);
  });
}, 100);
