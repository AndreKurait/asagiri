// page: setup
window.asagiri.on({ type: 'setup', step: 'choose', existingUbuntu: true,
  existingDistros: ['Managed241'], linux: null, sizes: {}, tasks: [], log: [] });
setTimeout(() => {
  const choices = [...document.querySelectorAll('input[name="linux"]')];
  const managed = choices.find(input => input.value === 'existing:Managed241');
  if (managed) {
    managed.click();
    document.querySelector('[data-act="choose"]').click();
  }
  const selected = sent.find(message => message.cmd === 'setup-choose');
  report([(selected?.value === 'existing:Managed241' ? 'PASS' : 'FAIL') +
    ' Setup submits the selected registered WSL distro']);
}, 100);
