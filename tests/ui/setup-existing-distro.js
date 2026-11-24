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
  report([(matches ? 'PASS' : 'FAIL') + ' Setup submits the second registered WSL distro by name']);
}, 100);
